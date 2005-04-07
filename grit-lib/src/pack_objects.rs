//! Negotiation-driven pack-objects: enumerate wants minus haves, delta search, stream PACK v2.
//!
//! Holds object metadata (not payloads) during planning; reads object bytes from the
//! [`Odb`] on demand when encoding.

use crate::environment::RepositoryOptions;

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::Write;

use flate2::write::ZlibEncoder;
use flate2::Compression;

use crate::delta_encode::encode_prefix_extension_delta;
use crate::error::{Error, Result};
use crate::objects::{parse_commit, parse_tag, parse_tree, HashAlgo, Object, ObjectId, ObjectKind};
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
                have_shallow_grafts: Some(&self.have_shallow),
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
                &selection.blob_name_hashes,
                self.opts.search_threads,
            )?;
            self.fire(PackProgress::PlannedDeltas);
            let delta_count = plan.entries.iter().filter(|e| e.base.is_some()).count() as u32;
            let pack = crate::pack_delta::serialize_pack_with_deltas(
                self.source_odb,
                &plan.entries,
                &opts,
                self.opts.write_threads,
            )?;
            w.write_all(&pack).map_err(Error::Io)?;
            let count = u32::from_be_bytes([pack[8], pack[9], pack[10], pack[11]]);
            return Ok(PackStats {
                object_count: count,
                pack_bytes: pack.len() as u64,
                delta_count,
            });
        };
        w.write_all(&bytes).map_err(Error::Io)?;
        let count = u32::from_be_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]);
        Ok(PackStats {
            object_count: count,
            pack_bytes: bytes.len() as u64,
            delta_count: 0,
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
    build_pack_with_shallow_and_filter(
        odb,
        wants,
        haves,
        &HashSet::new(),
        &HashSet::new(),
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

/// Same as [`build_pack_with_shallow_and_filter`] with a live [`Repository`] (for upload-pack).
pub fn build_pack_for_upload(
    repo: &Repository,
    wants: &[ObjectId],
    haves: &[ObjectId],
    have_shallow: &HashSet<ObjectId>,
    source_shallow: &HashSet<ObjectId>,
    filter: Option<&ObjectFilter>,
    opts: &PackBuildOptions,
) -> Result<Vec<u8>> {
    build_pack_with_shallow_and_filter(
        repo.odb(),
        wants,
        haves,
        have_shallow,
        source_shallow,
        filter,
        opts,
    )
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
    let plan = plan_deltas(odb, send, have_closure, opts, blob_name_hashes, Some(1))?;
    crate::pack_delta::serialize_pack_with_deltas(odb, &plan.entries, opts, Some(1))
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
    let have_closure = reachable_closure(odb, haves, &HashSet::new(), true, &HashSet::new())?;
    let selection = crate::pack_object_select::enumerate_pack_objects(
        odb,
        wants,
        haves,
        &crate::pack_object_select::PackEnumerateOptions {
            filter: None,
            shallow_grafts: &HashSet::new(),
            have_shallow_grafts: Some(&HashSet::new()),
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
    let have_closure = reachable_closure(odb, haves, &HashSet::new(), true, &HashSet::new())?;
    let selection = crate::pack_object_select::enumerate_pack_objects(
        odb,
        wants,
        haves,
        &crate::pack_object_select::PackEnumerateOptions {
            filter: None,
            shallow_grafts: &HashSet::new(),
            have_shallow_grafts: Some(&HashSet::new()),
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
    if !opts.delta {
        return serialize_pack(odb, send, opts);
    }
    let plan = plan_deltas(
        odb,
        send,
        have_closure,
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

/// The full delta plan: the ordered entries to emit plus the set of external
/// (thin) bases that were referenced but deliberately not emitted.
struct DeltaPlan {
    entries: Vec<PlannedEntry>,
    #[allow(dead_code)]
    external_bases: HashSet<ObjectId>,
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
/// peer's `haves` (`have_closure`) even though that base is not emitted; the base
/// oid is recorded in [`DeltaPlan::external_bases`] and referenced via REF_DELTA.
fn plan_deltas(
    odb: &Odb,
    send: &[ObjectId],
    have_closure: &HashSet<ObjectId>,
    opts: &PackBuildOptions,
    blob_name_hashes: &HashMap<ObjectId, u32>,
    search_threads: Option<usize>,
) -> Result<DeltaPlan> {
    // Load every object once. The plan keeps payloads so the serializer needn't
    // re-read; for the typical pack sizes this is the same data the whole-object
    // path would touch anyway.
    let mut objects: HashMap<ObjectId, Object> = HashMap::new();
    for &oid in send {
        objects.insert(oid, odb.read(&oid)?);
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
    if opts.reuse_deltas && odb.hash_algo() == HashAlgo::Sha1 {
        let objects_dir = odb.objects_dir();
        let candidates = crate::pack::collect_packed_delta_reuse_for_send(
            objects_dir,
            &in_pack,
            &in_pack,
            &objects,
        )?;
        for (t, (base, size, pack_bytes, zlib_start, zlib_end)) in candidates {
            if objects[&t].data.is_empty() {
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
        crate::pack_delta::extend_send_order_blob_deltas(
            send,
            &objects,
            blob_name_hashes,
            &islands,
            &mut delta_to_base,
            &reuse_delta_bases,
            odb.hash_algo().len(),
        );
        let skip_targets: HashSet<ObjectId> = delta_to_base.keys().copied().collect();
        let window_found = crate::pack_delta::search_windowed_blob_deltas(
            odb,
            crate::pack_delta::DeltaSearchInput {
                objects: &objects,
                blob_name_hashes,
                islands: &islands,
                skip_targets: &skip_targets,
                skip_as_target: &reuse_delta_bases,
                warmup_delta_edges: &delta_to_base,
                opts,
            },
            search_threads,
        )?;
        for (target, base) in window_found {
            delta_to_base.entry(target).or_insert(base);
        }

        if opts.thin {
            let hash_len = odb.hash_algo().len();
            let mut external_blob_data: HashMap<ObjectId, Vec<u8>> = HashMap::new();
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
                let obj = &objects[&t];
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
                    external_bases.insert(base);
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
        let obj = &objects[&oid];
        entries.push(PlannedEntry {
            oid,
            kind: obj.kind,
            data: obj.data.clone(),
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
    let Ok(repo) = crate::repo::Repository::open(git_dir, &RepositoryOptions::empty()) else {
        return crate::delta_islands::DeltaIslands::default();
    };
    let env = crate::environment::Environment::empty();
    let cfg = crate::config::ConfigSet::load(&env, Some(git_dir), true).unwrap_or_default();
    crate::delta_islands::load_delta_islands(&repo, &cfg, in_pack)
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
