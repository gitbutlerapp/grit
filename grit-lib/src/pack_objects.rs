//! Negotiation-driven pack-objects: enumerate wants minus haves, delta search, stream PACK v2.
//!
//! Holds object metadata (not payloads) during planning; reads object bytes from the
//! [`Odb`] on demand when encoding.

use crate::environment::RepositoryOptions;

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::Write;
use std::path::Path;

use flate2::write::ZlibEncoder;
use flate2::Compression;

use crate::bitmap_walk::ReachableSet;
use crate::delta_encode::{encode_prefix_extension_delta, DeltaIndex};
use crate::error::{Error, Result};
use crate::hash::HashingWriter;
use crate::objects::{
    empty_object_id_hash_set, oid_hash_map, oid_hash_set, oid_hash_set_from, parse_commit,
    parse_tag, HashAlgo, ObjectId, ObjectInfo, ObjectKind, OidHashMap, OidHashSet, TreeRefIter,
};
use crate::odb::Odb;
use crate::pack_object_select::{append_included_tags, HaveBoundary, NameTrees, TagCandidate};
use crate::parallel::{try_par_map_force, ParallelHashError};
use crate::progress::CancelSignal;
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
    pub fn write_to(self, w: &mut dyn Write) -> Result<PackStats> {
        self.write_to_with_cancel(w, &CancelSignal::never())
    }

    /// [`Self::write_to`], stopping with [`Error::Cancelled`] once `stop` fires.
    pub(crate) fn write_to_with_cancel(
        mut self,
        w: &mut dyn Write,
        stop: &CancelSignal<'_>,
    ) -> Result<PackStats> {
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
            false,
            stop,
        )?;
        let send = selection.send;
        self.fire(PackProgress::Enumerated { total: send.len() });
        if !opts.delta {
            let bytes = serialize_pack(self.source_odb, &send, &opts)?;
            stop.poll()?;
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
            stop,
        )?;
        self.fire(PackProgress::PlannedDeltas);
        crate::pack_delta::write_pack_with_deltas(
            self.source_odb,
            &plan.entries,
            &opts,
            self.opts.write_threads,
            w,
            None,
            stop,
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
/// their full commit closure before enumeration. Collects
/// [`write_pack_for_push`] into memory; pushes stream with that instead.
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
    let mut pack = Vec::new();
    write_pack_for_push(odb, wants, haves, opts, &mut pack)?;
    Ok(pack)
}

/// Stream the pack a push sends to `w`: the objects reachable from `wants` but not
/// from `haves`, bounded by the have tips as in [`build_pack_for_push`].
///
/// Memory stays bounded by the object list rather than the pack size: on-disk
/// deltas and whole objects are copied from the source packs batch by batch, and
/// every phase shares one mapping of each source pack.
///
/// # Parameters
///
/// - `odb`: the object database the objects are read from.
/// - `wants`: the new tips the remote should end up with.
/// - `haves`: objects the remote already holds (its tips and `.have`s), all present in `odb`.
/// - `opts`: delta, thin-pack and offset-delta settings for the remote.
/// - `w`: where the PACK v2 stream, trailer included, is written.
///
/// # Returns
///
/// Object and byte counts of the pack written.
///
/// # Errors
///
/// Missing wants, corrupt objects, or [`Error::Io`] when writing to `w` fails.
pub fn write_pack_for_push(
    odb: &Odb,
    wants: &[ObjectId],
    haves: &[ObjectId],
    opts: &PackBuildOptions,
    w: &mut dyn Write,
) -> Result<PackStats> {
    write_pack_for_push_with_cancel(odb, wants, haves, opts, w, &CancelSignal::never())
}

/// [`write_pack_for_push`], stopping with [`Error::Cancelled`] once `stop` fires.
pub(crate) fn write_pack_for_push_with_cancel(
    odb: &Odb,
    wants: &[ObjectId],
    haves: &[ObjectId],
    opts: &PackBuildOptions,
    w: &mut dyn Write,
    stop: &CancelSignal<'_>,
) -> Result<PackStats> {
    // One pack read context for the walk, the reuse scan and the write, so the
    // source packs are mapped and indexed once rather than once per phase.
    odb.with_pack_read_context(|| {
        write_pack_with_shallow_and_filter(
            odb,
            wants,
            haves,
            empty_object_id_hash_set(),
            empty_object_id_hash_set(),
            None,
            opts,
            HaveBoundary::TipsOnly,
            &[],
            w,
            stop,
        )
    })
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
        &[],
        &mut pack,
        &CancelSignal::never(),
    )?;
    Ok(pack)
}

/// Streaming form of [`build_pack_with_shallow_and_filter`]; `include_tags` are added
/// when the objects they peel to are selected.
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
    include_tags: &[TagCandidate],
    w: &mut dyn Write,
    stop: &CancelSignal<'_>,
) -> Result<PackStats> {
    let mut selection = crate::pack_object_select::enumerate_pack_objects_naming(
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
        false,
        stop,
    )?;
    append_included_tags(odb, &mut selection.send, include_tags)?;
    if !opts.delta {
        let bytes = serialize_pack(odb, &selection.send, opts)?;
        stop.poll()?;
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
        stop,
    )?;
    crate::pack_delta::write_pack_with_deltas(odb, &plan.entries, opts, None, w, None, stop, None)
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
    write_upload_pack_stream_with_cancel(
        repo,
        wants,
        haves,
        source_shallow,
        have_shallow,
        filter,
        &[],
        opts,
        progress,
        w,
        &CancelSignal::never(),
    )
}

/// [`write_upload_pack_stream`], stopping object enumeration when `stop` fires, that
/// also packs each of `include_tags` whose peeled object is among the objects sent
/// (`include-tag`), on the walk and reuse paths alike.
#[allow(clippy::too_many_arguments)]
pub(crate) fn write_upload_pack_stream_with_cancel(
    repo: &Repository,
    wants: &[ObjectId],
    haves: &[ObjectId],
    source_shallow: &HashSet<ObjectId>,
    have_shallow: &HashSet<ObjectId>,
    filter: Option<&ObjectFilter>,
    include_tags: &[TagCandidate],
    opts: &PackBuildOptions,
    progress: &mut dyn FnMut(PackProgress),
    w: &mut dyn Write,
    stop: &CancelSignal<'_>,
) -> Result<PackStats> {
    repo.odb().with_pack_read_context(|| {
        write_upload_pack_stream_in_context(
            repo,
            wants,
            haves,
            source_shallow,
            have_shallow,
            filter,
            include_tags,
            opts,
            progress,
            w,
            stop,
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
    include_tags: &[TagCandidate],
    opts: &PackBuildOptions,
    progress: &mut dyn FnMut(PackProgress),
    w: &mut dyn Write,
    stop: &CancelSignal<'_>,
) -> Result<PackStats> {
    if opts.use_bitmaps && filter.is_none() {
        if let Some(stats) = crate::pack_reuse::try_write_streaming_pack_including_tags(
            repo,
            wants,
            haves,
            source_shallow,
            have_shallow,
            filter,
            include_tags,
            opts,
            progress,
            w,
        )? {
            return Ok(stats);
        }
    }
    // The client's history only has to be painted down to where the want walk meets
    // it, not pre-walked to its roots.
    write_pack_with_shallow_and_filter(
        repo.odb(),
        wants,
        haves,
        have_shallow,
        source_shallow,
        filter,
        opts,
        HaveBoundary::TipsOnly,
        include_tags,
        w,
        stop,
    )
}

type PackIndexRows = Vec<(ObjectId, u64, u32)>;

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
    let algo = odb.hash_algo();
    let mut out = HashingWriter::new(algo, w);
    let reuse_stats = if let Some(reuse) = reuse {
        let (_, body_bytes) = write_delta_plan_stream_body(&mut out, odb, plan, opts, Some(reuse))?;
        out.finish().map_err(Error::Io)?;
        let pack_bytes = 12u64 + body_bytes + algo.len() as u64;
        let stats = reuse.take_stats();
        (pack_bytes, stats)
    } else {
        let (_, body_bytes) = write_delta_plan_stream_body(&mut out, odb, plan, opts, None)?;
        out.finish().map_err(Error::Io)?;
        (
            12u64 + body_bytes + algo.len() as u64,
            crate::pack_reuse::ReuseWriteStats::default(),
        )
    };
    let delta_count = plan.entries.iter().filter(|e| e.base.is_some()).count() as u32;
    Ok(PackStats {
        object_count: u32::try_from(plan.entries.len())
            .map_err(|_| Error::CorruptObject("pack object count exceeds u32".to_owned()))?,
        pack_bytes: reuse_stats.0,
        delta_count,
        verbatim_reuse_bytes: reuse_stats.1.verbatim_reuse_bytes,
        verbatim_reuse_objects: reuse_stats.1.verbatim_reuse_objects,
        pack_ofs_fixup_objects: reuse_stats.1.pack_ofs_fixup_objects,
        pack_ref_from_ofs_objects: reuse_stats.1.pack_ref_from_ofs_objects,
    })
}

fn write_delta_plan_stream_body(
    w: &mut dyn Write,
    odb: &Odb,
    plan: &DeltaPlan,
    opts: &PackBuildOptions,
    reuse: Option<&mut crate::pack_reuse::StreamingReuse>,
) -> Result<(PackIndexRows, u64)> {
    let count = u32::try_from(plan.entries.len())
        .map_err(|_| Error::CorruptObject("pack object count exceeds u32".to_owned()))?;
    write_pack_header(w, count)?;
    let mut index_rows = Vec::with_capacity(plan.entries.len());
    let body_bytes = write_planned_entries(w, odb, plan, opts, 12, reuse, &mut index_rows)?;
    Ok((index_rows, body_bytes))
}

const PACK_STREAM_COPY_CHUNK: usize = 64 * 1024;

pub(crate) struct CrcWriter<'a, W: Write + ?Sized> {
    inner: &'a mut W,
    crc: crc32fast::Hasher,
}

impl<'a, W: Write + ?Sized> CrcWriter<'a, W> {
    pub(crate) fn new(inner: &'a mut W, prefix: &[u8]) -> Self {
        let mut crc = crc32fast::Hasher::new();
        crc.update(prefix);
        Self { inner, crc }
    }

    pub(crate) fn finalize(self) -> u32 {
        self.crc.finalize()
    }
}

impl<W: Write + ?Sized> Write for CrcWriter<'_, W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.crc.update(buf);
        self.inner.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

struct ByteCounter<'a, W: Write + ?Sized> {
    inner: &'a mut W,
    n: u64,
}

impl<W: Write + ?Sized> Write for ByteCounter<'_, W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.n += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

fn write_all_chunked(w: &mut dyn Write, data: &[u8]) -> Result<()> {
    for chunk in data.chunks(PACK_STREAM_COPY_CHUNK) {
        w.write_all(chunk).map_err(Error::Io)?;
    }
    Ok(())
}

fn write_planned_entries(
    w: &mut dyn Write,
    odb: &Odb,
    plan: &DeltaPlan,
    opts: &PackBuildOptions,
    pack_body_start: u64,
    mut reuse: Option<&mut crate::pack_reuse::StreamingReuse>,
    index_rows: &mut Vec<(ObjectId, u64, u32)>,
) -> Result<u64> {
    let algo = odb.hash_algo();
    let compression = odb.pack_compression()?;
    let mut oid_to_offset: OidHashMap<u64> = oid_hash_map();
    let mut body_written = 0u64;

    for entry in &plan.entries {
        let start = pack_body_start + body_written;
        let mut chunk = Vec::new();
        if entry.base.is_none() {
            let mut crc_w = CrcWriter {
                inner: w,
                crc: crc32fast::Hasher::new(),
            };
            let wrote = {
                let mut counter = ByteCounter {
                    inner: &mut crc_w,
                    n: 0,
                };
                if let Some(reuse) = reuse.as_mut() {
                    if let Some(n) = reuse.try_write_whole(&mut counter, &entry.oid, opts)? {
                        n
                    } else {
                        write_whole_pack_object_to_odb(
                            &mut counter,
                            odb,
                            entry.oid,
                            entry.kind,
                            opts,
                        )?
                    }
                } else {
                    write_whole_pack_object_to_odb(&mut counter, odb, entry.oid, entry.kind, opts)?
                }
            };
            let crc = crc_w.finalize();
            index_rows.push((entry.oid, start, crc));
            body_written += wrote;
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
    Ok(body_written)
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
        false,
        &CancelSignal::never(),
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
        false,
        &CancelSignal::never(),
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
        &CancelSignal::never(),
    )?;
    crate::pack_delta::serialize_pack_with_deltas(odb, &plan.entries, opts, write_threads)
}

/// Stream a pack of exactly `send` (objects the receiver lacks, none of which it
/// needs as a delta base) to `w`, stopping with [`Error::Cancelled`] once `stop` fires.
///
/// Reuses on-disk whole objects and deltas as `opts` allows, like
/// [`build_pack_from_send_list`], without holding the pack in memory.
pub(crate) fn write_pack_from_send_list(
    odb: &Odb,
    send: &[ObjectId],
    opts: &PackBuildOptions,
    name_hashes: &HashMap<ObjectId, u32>,
    stop: &CancelSignal<'_>,
    w: &mut dyn Write,
    mut on_objects: Option<&mut dyn FnMut(u64)>,
) -> Result<PackStats> {
    if !opts.delta {
        let bytes = serialize_pack(odb, send, opts)?;
        stop.poll()?;
        w.write_all(&bytes).map_err(Error::Io)?;
        if let Some(on) = on_objects.as_deref_mut() {
            on(send.len() as u64);
        }
        return Ok(PackStats {
            object_count: u32::try_from(send.len()).unwrap_or(u32::MAX),
            pack_bytes: bytes.len() as u64,
            ..PackStats::default()
        });
    }
    odb.with_pack_read_context(|| {
        read_object_infos_with_progress(odb, send, None, stop, &mut on_objects)?;
        stop.poll()?;
        let plan = plan_deltas(
            odb,
            send,
            &HashSet::new(),
            None,
            opts,
            name_hashes,
            None,
            stop,
        )?;
        stop.poll()?;
        crate::pack_delta::write_pack_with_deltas(
            odb,
            &plan.entries,
            opts,
            None,
            w,
            None,
            stop,
            on_objects,
        )
    })
}

/// Stream a pack for a local transfer to `w`: enumerate haves on `have_odb`, read
/// payloads from `source_odb`, and stop with [`Error::Cancelled`] once `stop` fires.
#[allow(clippy::too_many_arguments)]
pub(crate) fn write_pack_for_local_fetch(
    source_odb: &Odb,
    wants: &[ObjectId],
    have_odb: &Odb,
    haves: &[ObjectId],
    source_shallow: &HashSet<ObjectId>,
    have_shallow: &HashSet<ObjectId>,
    opts: &PackBuildOptions,
    stop: &CancelSignal<'_>,
    w: &mut dyn Write,
) -> Result<PackStats> {
    PackObjects::new(have_odb, PackObjectsOptions::from(*opts))
        .with_source_odb(source_odb)
        .wants(wants)
        .haves(haves)
        .have_shallow_grafts(have_shallow)
        .source_shallow_grafts(source_shallow)
        .write_to_with_cancel(w, stop)
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
    write_whole_pack_object_to_odb(buf, odb, oid, kind, opts).map(|_| ())
}

/// Write one whole object from the ODB to `w`, reusing on-disk pack bytes or streaming
/// blobs at or above [`Odb::big_file_threshold`].
pub(crate) fn write_whole_pack_object_to_odb(
    w: &mut dyn Write,
    odb: &Odb,
    oid: ObjectId,
    kind: ObjectKind,
    opts: &PackBuildOptions,
) -> Result<u64> {
    if opts.reuse_objects && odb.hash_algo() == HashAlgo::Sha1 {
        if let Some((bytes, start, end)) =
            crate::pack::packed_full_object_range(odb.objects_dir(), &oid)?
        {
            write_all_chunked(w, &bytes[start..end])?;
            return Ok((end - start) as u64);
        }
    }
    let size = odb.read_info(&oid)?.size;
    let stream_from = odb.big_file_threshold()?;
    if kind == ObjectKind::Blob && size >= stream_from {
        let mut counter = ByteCounter { inner: w, n: 0 };
        odb.with_stream(&oid, |mut stream| {
            let payload_len = usize::try_from(stream.size).map_err(|_| {
                Error::CorruptObject(format!("object size overflow for {}", oid.to_hex()))
            })?;
            encode_pack_object_header_to(&mut counter, pack_type_code(kind), payload_len)?;
            crate::odb::deflate_stream_to_writer(
                &mut stream.reader,
                stream.size,
                &mut counter,
                odb.pack_compression()?,
            )?;
            Ok(())
        })?
        .ok_or_else(|| Error::ObjectNotFound(oid.to_hex()))?;
        return Ok(counter.n);
    }
    let obj = odb.read(&oid)?;
    write_whole_pack_object_to(w, odb, oid, kind, &obj.data, opts)
}

fn encode_pack_object_header_to(w: &mut dyn Write, type_code: u8, size: usize) -> Result<()> {
    let mut header = Vec::new();
    encode_pack_object_header(&mut header, type_code, size);
    w.write_all(&header).map_err(Error::Io)?;
    Ok(())
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
    write_whole_pack_object_to(buf, odb, oid, kind, data, opts).map(|_| ())
}

fn write_whole_pack_object_to(
    w: &mut dyn Write,
    odb: &Odb,
    oid: ObjectId,
    kind: ObjectKind,
    data: &[u8],
    opts: &PackBuildOptions,
) -> Result<u64> {
    let mut counter = ByteCounter { inner: w, n: 0 };
    if opts.reuse_objects && odb.hash_algo() == HashAlgo::Sha1 {
        if let Some((bytes, start, end)) =
            crate::pack::packed_full_object_range(odb.objects_dir(), &oid)?
        {
            write_all_chunked(&mut counter, &bytes[start..end])?;
            return Ok(counter.n);
        }
    }
    encode_pack_object_header_to(&mut counter, pack_type_code(kind), data.len())?;
    write_zlib_to(&mut counter, data, odb.pack_compression()?)?;
    Ok(counter.n)
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
    let mut none: Option<&mut dyn FnMut(u64)> = None;
    read_object_infos_with_progress(odb, oids, search_threads, &CancelSignal::never(), &mut none)
}

fn read_object_infos_with_progress(
    odb: &Odb,
    oids: &[ObjectId],
    search_threads: Option<usize>,
    stop: &CancelSignal<'_>,
    on_objects: &mut Option<&mut dyn FnMut(u64)>,
) -> Result<Vec<ObjectInfo>> {
    let par = crate::pack_delta::pack_delta_search_parallel_options(odb, search_threads)?;
    let read = |oid: &ObjectId| odb.read_info(oid);
    let stride = crate::progress::OBJECT_PROGRESS_STRIDE as usize;
    if par.threads.get() <= 1 || oids.len() < 32 {
        let mut out = Vec::with_capacity(oids.len());
        for (i, oid) in oids.iter().enumerate() {
            stop.poll()?;
            out.push(read(oid)?);
            if (i + 1).is_multiple_of(stride) {
                if let Some(on) = on_objects.as_deref_mut() {
                    on(stride as u64);
                }
            }
        }
        let rem = oids.len() % stride;
        if rem != 0 {
            if let Some(on) = on_objects.as_deref_mut() {
                on(rem as u64);
            }
        }
        return Ok(out);
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
/// Chains are depth-limited on [`crate::pack::apply_delta_depth_limit`]'s rule.
/// Correctness (re-indexability) is preserved because the chosen edges form no cycle,
/// even when on-disk deltas come from several packs, and every base is either in-pack
/// (emitted ahead of its targets) or, for thin packs, peer-held.
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
/// When `opts.thin`, trees and blobs may also delta against objects the receiver
/// holds, which are referenced via `REF_DELTA` and not emitted: the objects at the
/// same paths in the trees of the edge commits (Git's preferred bases, see
/// [`crate::pack_thin_bases::find_thin_bases`]), and for on-disk delta reuse any base
/// in `peer_held`. Edge commits are the parents of sent commits that `peer_commits`
/// (the selection's [`crate::pack_object_select::PackObjectSelection::have_closure`])
/// or `peer_held` contains.
#[allow(clippy::too_many_arguments)]
pub(crate) fn plan_deltas(
    odb: &Odb,
    send: &[ObjectId],
    peer_commits: &HashSet<ObjectId>,
    peer_held: Option<&ReachableSet>,
    opts: &PackBuildOptions,
    name_hashes: &HashMap<ObjectId, u32>,
    search_threads: Option<usize>,
    stop: &CancelSignal<'_>,
) -> Result<DeltaPlan> {
    let has_local_packs = objects_dir_has_local_packs(odb.objects_dir());
    let reuse_on_disk = opts.reuse_deltas && odb.hash_algo() == HashAlgo::Sha1 && has_local_packs;
    let search = opts.window > 0 && opts.max_depth > 0;
    stop.poll()?;
    let infos = read_object_infos(odb, send, search_threads)?;
    stop.poll()?;
    let in_pack: HashSet<ObjectId> = if reuse_on_disk || opts.thin || opts.respect_islands {
        send.iter().copied().collect()
    } else {
        HashSet::new()
    };

    // Delta islands (`--delta-islands`): only loaded when requested AND a git dir
    // is attached to the odb. An inactive island set (no `pack.island` config, or
    // no matched ref) imposes no restriction, so the common case is unaffected.
    let islands = load_islands_for_pack(odb, &in_pack, opts);

    // A thin pack may delta against what the receiver holds at the paths it changes.
    let peer_has = |oid: &ObjectId| peer_held.is_some_and(|held| held.contains(oid));
    let thin_bases = if opts.thin && (reuse_on_disk || search) {
        crate::pack_thin_bases::find_thin_bases(
            odb,
            send,
            &infos,
            &|oid| in_pack.contains(oid),
            &|oid| peer_commits.contains(oid) || peer_has(oid),
            name_hashes,
            opts.window.max(1),
        )?
    } else {
        crate::pack_thin_bases::ThinBases::default()
    };

    // target oid -> base oid (the object `target` deltas against).
    let mut delta_to_base: OidHashMap<ObjectId> = oid_hash_map();
    // Deltas whose instruction stream is reused verbatim from an existing pack.
    let mut reused_zlib: HashMap<ObjectId, crate::pack::OnDiskReuseZlibEntry> = HashMap::new();

    // On-disk delta reuse runs even with `--window=0` (no new deltas), matching Git pack-objects.
    if reuse_on_disk {
        // A thin pack keeps an on-disk delta whose base the receiver already holds; it is
        // written as `REF_DELTA` and completed by the receiver.
        let reusable_base = |oid: &ObjectId| {
            in_pack.contains(oid) || (opts.thin && (thin_bases.oids.contains(oid) || peer_has(oid)))
        };
        reused_zlib = crate::pack::collect_packed_delta_reuse_for_send(
            odb.objects_dir(),
            &in_pack,
            &reusable_base,
        )?;
        stop.poll()?;
        reused_zlib.retain(|t, (base, ..)| {
            // A stored delta against some other object the receiver holds is kept only
            // when the search has no base at the object's own path to do better with.
            let far_peer_base = !in_pack.contains(base) && !thin_bases.oids.contains(base);
            !(far_peer_base && search && thin_bases.targets.contains(t))
                && *base != *t
                && islands.in_same_island(t, base)
        });
        delta_to_base.extend(reused_zlib.iter().map(|(t, (base, ..))| (*t, *base)));
        // An empty blob is always stored whole.
        for (oid, info) in send.iter().zip(&infos) {
            if info.kind == ObjectKind::Blob && info.size == 0 && reused_zlib.remove(oid).is_some()
            {
                delta_to_base.remove(oid);
            }
        }
    }

    // An object that is the base of a reused on-disk delta keeps its content as it is:
    // it may only become a delta against something the receiver holds (which cannot
    // close a cycle), and only with room left for the chains that depend on it. With
    // no such bases it stays whole (or keeps its own on-disk delta).
    let reuse_dependents: OidHashMap<usize> = {
        let mut longest: OidHashMap<usize> = oid_hash_map();
        for target in reused_zlib.keys() {
            let mut cur = *target;
            let mut depth = 0usize;
            while let Some(base) = delta_to_base.get(&cur) {
                depth += 1;
                cur = *base;
                if depth > delta_to_base.len() {
                    break;
                }
            }
            let entry = longest.entry(cur).or_insert(0);
            *entry = (*entry).max(depth);
        }
        longest
    };

    let mut searched: OidHashMap<(usize, Vec<u8>)> = oid_hash_map();
    if search {
        let found = search_new_deltas(
            odb,
            send,
            &infos,
            &thin_bases,
            |oid| {
                delta_to_base.contains_key(oid)
                    || (thin_bases.oids.is_empty() && reuse_dependents.contains_key(oid))
            },
            &reuse_dependents,
            name_hashes,
            &islands,
            opts,
            search_threads,
            has_local_packs,
            stop.flag(),
        )?;
        stop.poll()?;
        for (target, base, delta) in found {
            delta_to_base.insert(target, base);
            searched.insert(target, delta);
        }
    }
    drop((in_pack, thin_bases));

    if opts.max_depth > 0 && !delta_to_base.is_empty() {
        // Cap chain length. After snipping, any removed target reverts to whole. The
        // edges are acyclic: reused ones come without cycles, and the search neither
        // retargets a reused delta nor deltas the root of a reused chain against
        // anything in the pack.
        let mut delta_std: HashMap<ObjectId, ObjectId> = delta_to_base.drain().collect();
        crate::pack::cap_delta_chain_depth(&mut delta_std, opts.max_depth);
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
                Some(_) => (
                    searched.remove(&oid),
                    reused_zlib
                        .remove(&oid)
                        .map(|(_, size, bytes, start, end)| (size, bytes, start, end)),
                ),
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
///
/// `thin_bases` join the search as bases only: they are never targets and never
/// written, so a delta found against one names an object the receiver holds. An
/// object in `reuse_dependents` (the base of reused on-disk deltas, with the longest
/// chain that depends on it) is only paired with those bases, leaving room for that
/// chain under the depth limit.
#[allow(clippy::too_many_arguments)]
fn search_new_deltas(
    odb: &Odb,
    send: &[ObjectId],
    infos: &[ObjectInfo],
    thin_bases: &crate::pack_thin_bases::ThinBases,
    skip: impl Fn(&ObjectId) -> bool,
    reuse_dependents: &OidHashMap<usize>,
    name_hashes: &HashMap<ObjectId, u32>,
    islands: &crate::delta_islands::DeltaIslands,
    opts: &PackBuildOptions,
    search_threads: Option<usize>,
    has_local_packs: bool,
    stop: &std::sync::atomic::AtomicBool,
) -> Result<Vec<SearchedDelta>> {
    u32::try_from(send.len() + thin_bases.bases.len())
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
    let base_oids: Vec<ObjectId> = thin_bases.bases.iter().map(|b| b.oid).collect();
    let base_infos = read_object_infos(odb, &base_oids, search_threads)?;
    let mut candidates: Vec<crate::delta_search::DeltaCandidate> = ids
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
                dependent_depth: reuse_dependents.get(&oid).copied().unwrap_or(0),
                base_only: false,
            }
        })
        .collect();
    drop(packed);
    candidates.extend(
        (send.len()..)
            .zip(thin_bases.bases.iter().zip(&base_infos))
            .filter(|(_, (base, info))| info.kind == base.kind && info.size > 0)
            .map(|(id, (base, info))| crate::delta_search::DeltaCandidate {
                id: id as u32,
                oid: base.oid,
                kind: info.kind,
                size: info.size,
                name_hash: base.name_hash,
                whole_in_pack: None,
                in_pack: None,
                dependent_depth: 0,
                base_only: true,
            }),
    );
    let oid_of = |id: u32| {
        let id = id as usize;
        send.get(id)
            .copied()
            .unwrap_or_else(|| base_oids[id - send.len()])
    };
    let window_memory = crate::pack_delta::pack_config(odb)?.pack_window_memory_bytes();
    let threads = crate::pack_delta::pack_delta_search_parallel_options(odb, search_threads)?;
    let allowed = |target: &ObjectId, base: &ObjectId| {
        islands.in_same_island(target, base)
            && (!reuse_dependents.contains_key(target) || thin_bases.oids.contains(base))
    };
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
        &allowed,
        stop,
    )?;
    Ok(found
        .into_iter()
        .map(|d| (oid_of(d.target), oid_of(d.base), (d.delta_len, d.zlib)))
        .collect())
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
    write_zlib_to(buf, data, compression)
}

fn write_zlib_to(w: &mut dyn Write, data: &[u8], compression: Compression) -> Result<()> {
    let mut enc = ZlibEncoder::new(w, compression);
    enc.write_all(data).map_err(Error::Io)?;
    enc.finish().map_err(Error::Io)?;
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
            &CancelSignal::never(),
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
            false,
            &CancelSignal::never(),
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
            &CancelSignal::never(),
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

    /// A merge being sent whose two receiver-held parents hold an old and the new
    /// `f.txt`; the new blob is also sent, so a held tree names a sent object at the
    /// path being changed.
    struct PeerHoldsSentBlob {
        dir: tempfile::TempDir,
        repo: Repository,
        old: ObjectId,
        new: ObjectId,
        /// The parents and everything they reach, in write order.
        peer_objects: Vec<ObjectId>,
        peer_commits: HashSet<ObjectId>,
        send: Vec<ObjectId>,
        name_hashes: HashMap<ObjectId, u32>,
    }

    impl PeerHoldsSentBlob {
        fn new() -> Self {
            use crate::objects::{serialize_tree, TreeEntry};
            use crate::pack_name_hash::pack_name_hash;
            let dir = tempfile::TempDir::new().unwrap();
            let repo =
                Repository::init(dir.path(), &crate::repo::InitOptions::default()).expect("init");
            let odb = repo.odb();
            let tree = |entries: &[(&str, ObjectId)]| {
                let entries: Vec<TreeEntry> = entries
                    .iter()
                    .map(|&(name, oid)| TreeEntry {
                        mode: 0o100644,
                        name: name.as_bytes().to_vec(),
                        oid,
                    })
                    .collect();
                odb.write(ObjectKind::Tree, &serialize_tree(&entries))
                    .unwrap()
            };
            let commit = |tree: ObjectId, parents: &[ObjectId]| {
                let parents: String = parents
                    .iter()
                    .map(|p| format!("parent {}\n", p.to_hex()))
                    .collect();
                let body = format!(
                    "tree {}\n{parents}author A <a@example.com> 0 +0000\ncommitter A <a@example.com> 0 +0000\n\nc\n",
                    tree.to_hex()
                );
                odb.write(ObjectKind::Commit, body.as_bytes()).unwrap()
            };
            let lines: Vec<String> = (0..200).map(|i| format!("line {i} of f.txt")).collect();
            let old = odb
                .write(ObjectKind::Blob, lines.join("\n").as_bytes())
                .unwrap();
            let new = odb
                .write(
                    ObjectKind::Blob,
                    format!("{}\nmore\n", lines.join("\n")).as_bytes(),
                )
                .unwrap();
            let old_tree = tree(&[("f.txt", old)]);
            let new_tree = tree(&[("f.txt", new)]);
            let first = commit(old_tree, &[]);
            let second = commit(new_tree, &[]);
            let extra = odb.write(ObjectKind::Blob, b"extra\n").unwrap();
            let merge_tree = tree(&[("extra.txt", extra), ("f.txt", new)]);
            let merge = commit(merge_tree, &[first, second]);
            Self {
                dir,
                old,
                new,
                peer_objects: vec![old, new, old_tree, new_tree, first, second],
                peer_commits: HashSet::from([first, second]),
                send: vec![merge, merge_tree, new, extra],
                name_hashes: HashMap::from([
                    (new, pack_name_hash("f.txt")),
                    (extra, pack_name_hash("extra.txt")),
                ]),
                repo,
            }
        }

        fn thin_plan(&self) -> DeltaPlan {
            plan_deltas(
                self.repo.odb(),
                &self.send,
                &self.peer_commits,
                None,
                &PackBuildOptions::for_serve(None, true, true),
                &self.name_hashes,
                Some(1),
                &CancelSignal::never(),
            )
            .unwrap()
        }
    }

    #[test]
    fn thin_pack_never_deltas_a_sent_blob_against_itself() {
        let fx = PeerHoldsSentBlob::new();
        let plan = fx.thin_plan();
        assert_eq!(plan.entries.len(), fx.send.len());
        for entry in &plan.entries {
            assert_ne!(
                entry.base,
                Some(entry.oid),
                "a delta against itself never resolves"
            );
        }
        let new = plan.entries.iter().find(|e| e.oid == fx.new).unwrap();
        assert_eq!(new.base, Some(fx.old), "the other held version is the base");
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn thin_pack_with_held_sent_blob_completes_in_git() {
        let fx = PeerHoldsSentBlob::new();
        let plan = fx.thin_plan();
        let opts = PackBuildOptions::for_serve(None, true, true);
        let pack = crate::pack_delta::serialize_pack_with_deltas(
            fx.repo.odb(),
            &plan.entries,
            &opts,
            Some(1),
        )
        .unwrap();

        let root = fx.dir.path();
        grit_test_support::git(root, &["init", "-q", "--bare", "peer"]);
        let peer = root.join("peer");
        for oid in &fx.peer_objects {
            let obj = fx.repo.odb().read(oid).unwrap();
            let written = grit_test_support::git_cmd(&[
                "hash-object",
                "-w",
                "--stdin",
                "-t",
                obj.kind.as_str(),
            ])
            .in_dir(&peer)
            .stdin(obj.data)
            .suc();
            assert_eq!(written.stdout.trim(), oid.to_hex());
        }
        grit_test_support::git_cmd(&["index-pack", "--strict", "--fix-thin", "--stdin"])
            .in_dir(&peer)
            .stdin(pack.bytes)
            .suc();
        let merge = fx.send[0].to_hex();
        grit_test_support::git(&peer, &["update-ref", "refs/heads/main", &merge]);
        grit_test_support::git(&peer, &["fsck", "--strict", "--no-dangling"]);
    }
}
