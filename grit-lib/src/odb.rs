//! Loose object database: reading and writing zlib-compressed Git objects.
//!
//! Git stores objects as files under `<git-dir>/objects/<xx>/<38-hex-chars>`,
//! where the path is derived from the SHA-1 digest. Each file is a zlib-
//! compressed byte sequence whose decompressed form is:
//!
//! ```text
//! "<type> <size>\0<data>"
//! ```
//!
//! # Usage
//!
//! ```no_run
//! use std::path::Path;
//! use grit_lib::odb::Odb;
//!
//! let odb = Odb::new(Path::new(".git/objects"));
//! ```

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;
use flate2::Compression;
use sha1::{Digest, Sha1};
use sha2::Sha256;

use crate::config::ConfigSet;
use crate::error::{Error, Result};
use crate::midx::{midx_oid_listed_in_tip, try_read_object_via_midx};
use crate::objects::{HashAlgo, Object, ObjectId, ObjectKind};
use crate::pack;

/// Decompress a zlib-wrapped loose object payload from an open file.
///
/// When the zlib wrapper advertises a preset dictionary (FDICT), `flate2` typically fails with a
/// generic corrupt-stream error; map that to `"needs dictionary"` so callers match Git's messages
/// (`t1006-cat-file` zlib-dictionary test).
fn read_zlib_loose_payload(mut file: fs::File) -> Result<Vec<u8>> {
    let mut hdr = [0u8; 2];
    file.read_exact(&mut hdr).map_err(Error::Io)?;
    let cmf_flg = u16::from(hdr[0]) << 8 | u16::from(hdr[1]);
    let looks_like_zlib_header = cmf_flg != 0 && cmf_flg % 31 == 0;
    let preset_dictionary = looks_like_zlib_header && (hdr[1] & 0x20) != 0;
    let mut decoder = ZlibDecoder::new(hdr.as_slice().chain(file));
    let mut raw = Vec::new();
    match decoder.read_to_end(&mut raw) {
        Ok(_) => Ok(raw),
        Err(e) => {
            if preset_dictionary {
                Err(Error::Zlib("needs dictionary".to_owned()))
            } else {
                Err(Error::Zlib(e.to_string()))
            }
        }
    }
}

/// True when `oid` is stored as a loose object or in a **non-promisor** local pack.
fn exists_materialized_in_objects_dir(objects_dir: &Path, oid: &ObjectId) -> bool {
    let loose = objects_dir
        .join(oid.loose_prefix())
        .join(oid.loose_suffix());
    if loose.exists() {
        return true;
    }
    let Ok(indexes) = pack::read_local_pack_indexes_cached(objects_dir) else {
        return false;
    };
    for idx in &indexes {
        if idx.pack_path.with_extension("promisor").is_file() {
            continue;
        }
        if idx.contains(oid) {
            return true;
        }
    }
    false
}

/// A loose-object database rooted at a given `objects/` directory.
#[derive(Clone)]
pub struct Odb {
    objects_dir: PathBuf,
    /// Work tree root for resolving relative alternate env paths.
    work_tree: Option<PathBuf>,
    /// Embedded submodule object stores registered for this read pass (Git `register_all_submodule_sources`).
    submodule_alternate_dirs: Arc<Mutex<Vec<PathBuf>>>,
    /// When set, used to read `core.multiPackIndex` (and related) for MIDX-backed object reads.
    config_git_dir: Option<PathBuf>,
    /// Cache for `core.multiPackIndex` — populated on first lookup.
    ///
    /// Reading this config requires loading the system/global/local config cascade and reparsing
    /// every file; the value cannot change for a process that has opened a single repository, so
    /// caching it here avoids re-loading the cascade for every object read.
    core_multi_pack_index_cache: Arc<OnceLock<bool>>,
    /// When `Some`, object writes are redirected into this in-memory overlay instead of being
    /// persisted to the loose store (Git's tmp-objdir). Reads consult the overlay first. This
    /// mirrors `git merge-tree --quiet`, which performs a full merge but must leave the object
    /// database untouched (no new loose objects).
    mem_overlay: Arc<Mutex<Option<std::collections::HashMap<ObjectId, (ObjectKind, Vec<u8>)>>>>,
    /// The repository's object hash algorithm (`extensions.objectformat`),
    /// detected lazily from the config and cached. Determines the hash used
    /// when writing objects. Defaults to SHA-1 when no config is available.
    hash_algo_cache: Arc<OnceLock<HashAlgo>>,
    /// Zlib level for loose-object writes (`core.looseCompression` falling back to
    /// `core.compression`, Git default `Z_BEST_SPEED`), resolved lazily and cached.
    loose_zlib_cache: Arc<OnceLock<Compression>>,
}

impl std::fmt::Debug for Odb {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Odb")
            .field("objects_dir", &self.objects_dir)
            .field("work_tree", &self.work_tree)
            .field("submodule_alternate_dirs", &"<mutex>")
            .field("config_git_dir", &self.config_git_dir)
            .finish()
    }
}

impl Odb {
    /// Create an [`Odb`] pointing at the given `objects/` directory.
    ///
    /// The directory does not need to exist yet; it will be created on the
    /// first write operation.
    #[must_use]
    pub fn new(objects_dir: &Path) -> Self {
        Self {
            objects_dir: objects_dir.to_path_buf(),
            work_tree: None,
            submodule_alternate_dirs: Arc::new(Mutex::new(Vec::new())),
            config_git_dir: None,
            core_multi_pack_index_cache: Arc::new(OnceLock::new()),
            mem_overlay: Arc::new(Mutex::new(None)),
            hash_algo_cache: Arc::new(OnceLock::new()),
            loose_zlib_cache: Arc::new(OnceLock::new()),
        }
    }

    /// Create an [`Odb`] with a work tree for resolving relative alternate paths.
    #[must_use]
    pub fn with_work_tree(objects_dir: &Path, work_tree: &Path) -> Self {
        Self {
            objects_dir: objects_dir.to_path_buf(),
            work_tree: Some(work_tree.to_path_buf()),
            submodule_alternate_dirs: Arc::new(Mutex::new(Vec::new())),
            config_git_dir: None,
            core_multi_pack_index_cache: Arc::new(OnceLock::new()),
            mem_overlay: Arc::new(Mutex::new(None)),
            hash_algo_cache: Arc::new(OnceLock::new()),
            loose_zlib_cache: Arc::new(OnceLock::new()),
        }
    }

    /// Enable the in-memory write overlay (Git's tmp-objdir): subsequent [`Self::write`]/
    /// [`Self::write_local`] calls keep objects only in memory, and [`Self::read`] consults that
    /// overlay before the on-disk store. Used by `merge-tree --quiet` so a full merge can run
    /// without persisting any new loose objects.
    pub fn enable_mem_overlay(&self) {
        if let Ok(mut guard) = self.mem_overlay.lock() {
            *guard = Some(std::collections::HashMap::new());
        }
    }

    /// Disable the in-memory write overlay, discarding any objects accumulated in it.
    pub fn disable_mem_overlay(&self) {
        if let Ok(mut guard) = self.mem_overlay.lock() {
            *guard = None;
        }
    }

    /// Whether the in-memory write overlay is currently enabled.
    fn overlay_active(&self) -> bool {
        self.mem_overlay.lock().is_ok_and(|g| g.is_some())
    }

    /// If the in-memory overlay is active, store `(kind, data)` under `oid` there and return
    /// `true`; otherwise return `false` so the caller falls through to the on-disk path.
    fn overlay_store(&self, oid: ObjectId, kind: ObjectKind, data: &[u8]) -> bool {
        if let Ok(mut guard) = self.mem_overlay.lock() {
            if let Some(map) = guard.as_mut() {
                map.entry(oid).or_insert_with(|| (kind, data.to_vec()));
                return true;
            }
        }
        false
    }

    /// Read `oid` from the in-memory overlay, if active and present.
    fn overlay_read(&self, oid: &ObjectId) -> Option<Object> {
        if let Ok(guard) = self.mem_overlay.lock() {
            if let Some(map) = guard.as_ref() {
                if let Some((kind, data)) = map.get(oid) {
                    return Some(Object {
                        kind: *kind,
                        data: data.clone(),
                    });
                }
            }
        }
        None
    }

    /// Register `<submodule-git-dir>/objects` for every stage-0 gitlink in `index` that has a
    /// checkout under `work_tree`, so reads can resolve submodule commits stored only in the
    /// nested repository (matches Git's `odb_add_submodule_source_by_path` / `register_all_submodule_sources`).
    pub fn register_submodule_object_directories_from_index(
        &self,
        work_tree: &Path,
        index: &crate::index::Index,
    ) {
        use crate::diff::submodule_embedded_git_dir;

        let Ok(mut dirs) = self.submodule_alternate_dirs.lock() else {
            return;
        };
        dirs.clear();
        for e in &index.entries {
            if e.stage() != 0 || e.mode != crate::index::MODE_GITLINK {
                continue;
            }
            let path_str = String::from_utf8_lossy(&e.path);
            let abs = work_tree.join(path_str.as_ref());
            let Some(sub_git) = submodule_embedded_git_dir(&abs) else {
                continue;
            };
            let objects = sub_git.join("objects");
            if !objects.is_dir() {
                continue;
            }
            let canon = objects.canonicalize().unwrap_or(objects);
            if !dirs.iter().any(|p| p == &canon) {
                dirs.push(canon);
            }
        }
    }

    /// Attach a git directory so [`Self::read`] can honor `core.multiPackIndex` when resolving packed objects.
    #[must_use]
    pub fn with_config_git_dir(mut self, git_dir: PathBuf) -> Self {
        self.config_git_dir = Some(git_dir);
        self
    }

    /// The repository's object hash algorithm, detected from
    /// `extensions.objectformat` and cached for the lifetime of this `Odb`.
    ///
    /// The git directory is taken from the attached `config_git_dir` when set,
    /// otherwise inferred as the parent of the `objects/` directory. Defaults
    /// to [`HashAlgo::Sha1`] when no config can be read.
    #[must_use]
    pub fn hash_algo(&self) -> HashAlgo {
        *self.hash_algo_cache.get_or_init(|| {
            let git_dir = self
                .config_git_dir
                .clone()
                .or_else(|| self.objects_dir.parent().map(Path::to_path_buf));
            let Some(git_dir) = git_dir else {
                return HashAlgo::Sha1;
            };
            let cfg = ConfigSet::load(Some(&git_dir), true).unwrap_or_default();
            cfg.get("extensions.objectformat")
                .and_then(|v| HashAlgo::from_name(&v))
                .unwrap_or(HashAlgo::Sha1)
        })
    }

    /// Zlib compression used for loose-object writes, resolved from
    /// `core.looseCompression` / `core.compression` (Git `zlib_compression_level`, default
    /// `Z_BEST_SPEED`) and cached for the lifetime of this `Odb`.
    fn loose_compression(&self) -> Compression {
        *self.loose_zlib_cache.get_or_init(|| {
            let git_dir = self
                .config_git_dir
                .clone()
                .or_else(|| self.objects_dir.parent().map(Path::to_path_buf));
            let level = match git_dir {
                Some(git_dir) => ConfigSet::load(Some(&git_dir), true)
                    .unwrap_or_default()
                    .loose_objects_zlib_level(),
                None => 1,
            };
            Compression::new(level)
        })
    }

    fn core_multi_pack_index_enabled(&self) -> bool {
        // The system/global/local config cascade is expensive to load (the parser walks every
        // file from `/etc/gitconfig` through `.git/config`); calling it once per object lookup
        // dominated `status` runtime. Cache the result for the lifetime of this `Odb`.
        *self.core_multi_pack_index_cache.get_or_init(|| {
            let Some(git_dir) = &self.config_git_dir else {
                return false;
            };
            let cfg = ConfigSet::load(Some(git_dir), true).unwrap_or_default();
            match cfg.get_bool("core.multiPackIndex") {
                Some(Ok(b)) => b,
                Some(Err(_)) => true,
                None => true,
            }
        })
    }

    /// Return the path to the `objects/` directory.
    #[must_use]
    pub fn objects_dir(&self) -> &Path {
        &self.objects_dir
    }

    /// Return the attached git directory, if one was set with
    /// [`Self::with_config_git_dir`].
    ///
    /// Used by config- and ref-aware operations (delta islands, on-disk delta
    /// reuse) that need the repository root rather than just the object store.
    #[must_use]
    pub fn config_git_dir(&self) -> Option<&Path> {
        self.config_git_dir.as_deref()
    }

    /// Return the filesystem path for a given object ID.
    #[must_use]
    pub fn object_path(&self, oid: &ObjectId) -> PathBuf {
        self.objects_dir
            .join(oid.loose_prefix())
            .join(oid.loose_suffix())
    }

    /// Whether the object exists under this database directory only (loose or local packs).
    ///
    /// Unlike [`Self::exists`], this ignores `info/alternates` and
    /// `GIT_ALTERNATE_OBJECT_DIRECTORIES`. Used for partial-clone bookkeeping where
    /// objects reachable via alternates are still treated as "missing" until copied locally.
    ///
    /// Objects stored **only** in promisor packs (sibling `.promisor` marker next to the
    /// `.pack`) are treated as absent: Git considers them fetchable on demand, and
    /// `rev-list --missing=print` lists them until materialized as loose objects or a
    /// non-promisor pack.
    ///
    /// The empty tree object is treated as present without a loose file (matches Git).
    #[must_use]
    pub fn exists_local(&self, oid: &ObjectId) -> bool {
        const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
        if oid.to_hex() == EMPTY_TREE {
            return true;
        }
        exists_materialized_in_objects_dir(&self.objects_dir, oid)
    }

    /// Check whether an object exists in the loose store or any pack file.
    #[must_use]
    pub fn exists(&self, oid: &ObjectId) -> bool {
        // The empty tree is a well-known object (no on-disk loose file). Git's
        // canonical SHA-1 is `...8d69288fbee4904`; some harnesses still use the
        // legacy typo hash `...899d69f7c6948d4` — treat both as present.
        const EMPTY_TREE_CANON: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
        const EMPTY_TREE_LEGACY: &str = "4b825dc642cb6eb9a060e54bf899d69f7c6948d4";
        let hex = oid.to_hex();
        if hex == EMPTY_TREE_CANON || hex == EMPTY_TREE_LEGACY {
            return true;
        }
        if self.exists_in_dir(&self.objects_dir, oid) {
            return true;
        }
        // Check alternates from info/alternates file.
        if let Ok(alts) = pack::read_alternates_recursive(&self.objects_dir) {
            for alt_dir in &alts {
                if self.exists_in_dir(alt_dir, oid) {
                    return true;
                }
            }
        }
        // Check GIT_ALTERNATE_OBJECT_DIRECTORIES env var.
        for alt_dir in env_alternate_dirs(self.work_tree.as_deref()) {
            if self.exists_in_dir(&alt_dir, oid) {
                return true;
            }
        }
        if let Ok(guard) = self.submodule_alternate_dirs.lock() {
            for alt_dir in guard.iter() {
                if self.exists_in_dir(alt_dir, oid) {
                    return true;
                }
            }
        }
        false
    }

    /// Check whether an object exists in a specific objects directory.
    fn exists_in_dir(&self, objects_dir: &Path, oid: &ObjectId) -> bool {
        let loose = objects_dir
            .join(oid.loose_prefix())
            .join(oid.loose_suffix());
        if loose.exists() {
            return true;
        }
        if let Ok(indexes) = pack::read_local_pack_indexes_cached(objects_dir) {
            for idx in &indexes {
                if idx.contains(oid) {
                    return true;
                }
            }
        }
        if objects_dir == self.objects_dir.as_path()
            && self.config_git_dir.is_some()
            && self.core_multi_pack_index_enabled()
        {
            match midx_oid_listed_in_tip(objects_dir, oid) {
                Ok(Some(true)) => return true,
                Ok(Some(false)) | Ok(None) => {}
                Err(_) => return false,
            }
        }
        false
    }

    /// Touch the loose object file or pack file containing `oid`, matching Git's
    /// `odb_freshen_object` (updates mtime so age-based prune keeps recently re-referenced objects).
    ///
    /// Returns `true` if an on-disk object was found and touched.
    #[must_use]
    pub fn freshen_object(&self, oid: &ObjectId) -> bool {
        const EMPTY_TREE_CANON: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
        const EMPTY_TREE_LEGACY: &str = "4b825dc642cb6eb9a060e54bf899d69f7c6948d4";
        let hex = oid.to_hex();
        if hex == EMPTY_TREE_CANON || hex == EMPTY_TREE_LEGACY {
            return false;
        }

        let loose = self.object_path(oid);
        if loose.is_file() {
            return touch_path_mtime(&loose);
        }

        if freshen_object_in_objects_dir(&self.objects_dir, oid) {
            return true;
        }

        if let Ok(alts) = pack::read_alternates_recursive(&self.objects_dir) {
            for alt_dir in &alts {
                if freshen_object_in_objects_dir(alt_dir, oid) {
                    return true;
                }
            }
        }

        for alt_dir in env_alternate_dirs(self.work_tree.as_deref()) {
            if freshen_object_in_objects_dir(&alt_dir, oid) {
                return true;
            }
        }

        if let Ok(guard) = self.submodule_alternate_dirs.lock() {
            for alt_dir in guard.iter() {
                if freshen_object_in_objects_dir(alt_dir, oid) {
                    return true;
                }
            }
        }

        false
    }

    /// Read a loose object file at `path`, verifying the uncompressed payload hashes to `expected_oid`.
    ///
    /// Git stores loose objects under paths derived from the OID; if the file contents hash to a
    /// different id (for example after a mistaken `mv`), this returns [`Error::LooseHashMismatch`].
    ///
    /// # Errors
    ///
    /// - [`Error::Zlib`] — decompression failed.
    /// - [`Error::CorruptObject`] — header is malformed.
    /// - [`Error::LooseHashMismatch`] — payload OID does not match `expected_oid`.
    pub fn read_loose_verify_oid(path: &Path, expected_oid: &ObjectId) -> Result<Object> {
        let file = fs::File::open(path).map_err(Error::Io)?;
        let raw = read_zlib_loose_payload(file)?;
        let obj = parse_object_bytes_with_oid(&raw, expected_oid)?;
        // Verify against the expected OID using its own hash algorithm; a SHA-256
        // loose object must be re-hashed with SHA-256, not SHA-1.
        let computed = hash_object_data_with(expected_oid.algo(), obj.kind, &obj.data);
        if computed != *expected_oid {
            return Err(Error::LooseHashMismatch {
                path: path.display().to_string(),
                real_oid: computed.to_hex(),
            });
        }
        Ok(obj)
    }

    /// Read and decompress an object from the loose store.
    ///
    /// # Errors
    ///
    /// - [`Error::ObjectNotFound`] — no file at the expected path.
    /// - [`Error::Zlib`] — decompression failed.
    /// - [`Error::CorruptObject`] — header is malformed.
    pub fn read(&self, oid: &ObjectId) -> Result<Object> {
        // The empty tree is a well-known virtual object — no storage needed.
        const EMPTY_TREE_CANON: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
        const EMPTY_TREE_LEGACY: &str = "4b825dc642cb6eb9a060e54bf899d69f7c6948d4";
        let hex = oid.to_hex();
        if hex == EMPTY_TREE_CANON || hex == EMPTY_TREE_LEGACY {
            return Ok(crate::objects::Object {
                kind: crate::objects::ObjectKind::Tree,
                data: Vec::new(),
            });
        }

        // Objects written under an active in-memory overlay never hit disk, so they must be
        // resolved here before any loose/pack lookup.
        if let Some(obj) = self.overlay_read(oid) {
            return Ok(obj);
        }

        // Git prepares the packed object store (registering the packs the MIDX names) before
        // serving reads; a MIDX-referenced pack whose `.idx` cannot be opened reports
        // `packfile <pack> index unavailable` even when the requested object turns out to be
        // loose. Reproduce that once-per-process so `rev-list` over a corrupt idx still warns.
        if self.config_git_dir.is_some() && self.core_multi_pack_index_enabled() {
            crate::midx::validate_midx_referenced_packs(&self.objects_dir);
        }

        let path = self.object_path(oid);
        match fs::File::open(&path) {
            Ok(file) => {
                let raw = read_zlib_loose_payload(file)?;
                // Match Git: loose objects are read from the path implied by `oid` without
                // requiring the payload to hash back to that oid (t1006 corrupt-loose / swapped files).
                return parse_object_bytes(&raw);
            }
            Err(_) => {
                // Loose object not found; try pack files.
            }
        }

        if self.config_git_dir.is_some() && self.core_multi_pack_index_enabled() {
            if let Some(obj) = try_read_object_via_midx(&self.objects_dir, oid)? {
                return Ok(obj);
            }
        }

        // Fall back to pack files.
        match pack::read_object_from_packs(&self.objects_dir, oid) {
            Ok(obj) => return Ok(obj),
            Err(Error::ObjectNotFound(_)) => {}
            Err(err) => return Err(err),
        }

        let midx_alt = self.config_git_dir.is_some() && self.core_multi_pack_index_enabled();

        // Check alternates from info/alternates file.
        if let Ok(alts) = pack::read_alternates_recursive(&self.objects_dir) {
            for alt_dir in &alts {
                if let Ok(obj) = Self::read_from_dir(alt_dir, oid, midx_alt) {
                    return Ok(obj);
                }
            }
        }

        // Check GIT_ALTERNATE_OBJECT_DIRECTORIES env var.
        for alt_dir in env_alternate_dirs(self.work_tree.as_deref()) {
            if let Ok(obj) = Self::read_from_dir(&alt_dir, oid, midx_alt) {
                return Ok(obj);
            }
        }

        if let Ok(guard) = self.submodule_alternate_dirs.lock() {
            for alt_dir in guard.iter() {
                if let Ok(obj) = Self::read_from_dir(alt_dir, oid, false) {
                    return Ok(obj);
                }
            }
        }

        Err(Error::ObjectNotFound(oid.to_hex()))
    }

    /// Try to read an object from a specific objects directory (loose or pack).
    fn read_from_dir(objects_dir: &Path, oid: &ObjectId, use_midx: bool) -> Result<Object> {
        let loose = objects_dir
            .join(oid.loose_prefix())
            .join(oid.loose_suffix());
        if let Ok(file) = fs::File::open(&loose) {
            let raw = read_zlib_loose_payload(file)?;
            return parse_object_bytes(&raw);
        }
        if use_midx {
            if let Some(obj) = try_read_object_via_midx(objects_dir, oid)? {
                return Ok(obj);
            }
        }
        match pack::read_object_from_packs(objects_dir, oid) {
            Ok(obj) => Ok(obj),
            Err(Error::ObjectNotFound(_)) => Err(Error::ObjectNotFound(oid.to_hex())),
            Err(err) => Err(err),
        }
    }

    /// Hash raw content of a given kind with SHA-1 and return the [`ObjectId`].
    ///
    /// This does **not** write anything to disk. Prefer [`Self::hash`] when a
    /// repository hash algorithm is available, so SHA-256 repositories are
    /// handled correctly.
    #[must_use]
    pub fn hash_object_data(kind: ObjectKind, data: &[u8]) -> ObjectId {
        hash_object_data_with(HashAlgo::Sha1, kind, data)
    }

    /// Hash raw content of a given kind using this repository's hash algorithm.
    ///
    /// This does **not** write anything to disk.
    #[must_use]
    pub fn hash(&self, kind: ObjectKind, data: &[u8]) -> ObjectId {
        hash_object_data_with(self.hash_algo(), kind, data)
    }

    /// Write an object to the loose store and return its [`ObjectId`].
    ///
    /// If the object already exists it is not overwritten (Git behaviour).
    ///
    /// # Errors
    ///
    /// - [`Error::Io`] — could not create the directory or write the file.
    /// - [`Error::Zlib`] — compression failed.
    pub fn write(&self, kind: ObjectKind, data: &[u8]) -> Result<ObjectId> {
        let store_bytes = build_store_bytes(kind, data);
        let oid = hash_bytes_with(self.hash_algo(), &store_bytes);

        // Cheapest check first: a loose copy in this store answers every case below with a
        // single stat, avoiding the full pack/alternates/MIDX existence scan per write.
        let path = self.object_path(&oid);
        if path.exists() {
            let _ = self.freshen_object(&oid);
            return Ok(oid);
        }

        // When the in-memory overlay is active, keep the object in memory only (unless it is
        // already present on disk, in which case nothing new needs to be written anyway).
        if self.overlay_active() && !self.exists(&oid) && self.overlay_store(oid, kind, data) {
            return Ok(oid);
        }

        if self.exists(&oid) {
            let _ = self.freshen_object(&oid);
            return Ok(oid);
        }

        let prefix_dir = path
            .parent()
            .ok_or_else(|| Error::PathError("object path has no parent".to_owned()))?;
        fs::create_dir_all(prefix_dir)?;

        // Write to a temp file in the same directory, then rename atomically.
        let tmp_path = prefix_dir.join(format!("tmp_{}", oid.loose_suffix()));
        {
            let tmp_file = fs::File::create(&tmp_path)?;
            let mut encoder = ZlibEncoder::new(tmp_file, self.loose_compression());
            encoder
                .write_all(&store_bytes)
                .map_err(|e| Error::Zlib(e.to_string()))?;
            encoder.finish().map_err(|e| Error::Zlib(e.to_string()))?;
        }
        fs::rename(&tmp_path, &path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o444));
        }

        Ok(oid)
    }

    /// Write an object as a loose file in this object directory only.
    ///
    /// Unlike [`Self::write`], this ignores `info/alternates` and
    /// `GIT_ALTERNATE_OBJECT_DIRECTORIES`: if the object exists only in an
    /// alternate store, it is still written here. That matches how Git's
    /// `unpack-objects` materializes every packed object into the receiving
    /// repository even when the same OID is already reachable via alternates
    /// (see `t5519-push-alternates`).
    ///
    /// The well-known empty tree is still written when no loose file exists yet,
    /// even though [`Self::exists_local`] treats it as virtually present.
    ///
    /// # Errors
    ///
    /// Same as [`Self::write`].
    pub fn write_local(&self, kind: ObjectKind, data: &[u8]) -> Result<ObjectId> {
        let store_bytes = build_store_bytes(kind, data);
        let oid = hash_bytes_with(self.hash_algo(), &store_bytes);

        let path = self.object_path(&oid);
        if path.exists() {
            let _ = self.freshen_object(&oid);
            return Ok(oid);
        }
        if exists_materialized_in_objects_dir(&self.objects_dir, &oid) {
            let _ = self.freshen_object(&oid);
            return Ok(oid);
        }

        let prefix_dir = path
            .parent()
            .ok_or_else(|| Error::PathError("object path has no parent".to_owned()))?;
        fs::create_dir_all(prefix_dir)?;

        let tmp_path = prefix_dir.join(format!("tmp_{}", oid.loose_suffix()));
        {
            let tmp_file = fs::File::create(&tmp_path)?;
            let mut encoder = ZlibEncoder::new(tmp_file, self.loose_compression());
            encoder
                .write_all(&store_bytes)
                .map_err(|e| Error::Zlib(e.to_string()))?;
            encoder.finish().map_err(|e| Error::Zlib(e.to_string()))?;
        }
        fs::rename(&tmp_path, &path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o444));
        }

        Ok(oid)
    }

    /// Write a loose object file when it is missing, even if [`Self::exists`] is true because
    /// the object lives only in a pack.
    ///
    /// Used when materializing a partial-clone layout: objects must be duplicated as loose files
    /// before local packs are removed. Unlike [`Self::write_local`], objects present only in a
    /// promisor pack are still written because [`Self::exists_local`] treats those as absent.
    pub fn write_loose_materialize(&self, kind: ObjectKind, data: &[u8]) -> Result<ObjectId> {
        let store_bytes = build_store_bytes(kind, data);
        let oid = hash_bytes_with(self.hash_algo(), &store_bytes);
        let path = self.object_path(&oid);
        if path.exists() {
            let _ = self.freshen_object(&oid);
            return Ok(oid);
        }

        let prefix_dir = path
            .parent()
            .ok_or_else(|| Error::PathError("object path has no parent".to_owned()))?;
        fs::create_dir_all(prefix_dir)?;

        let tmp_path = prefix_dir.join(format!("tmp_{}", oid.loose_suffix()));
        {
            let tmp_file = fs::File::create(&tmp_path)?;
            let mut encoder = ZlibEncoder::new(tmp_file, self.loose_compression());
            encoder
                .write_all(&store_bytes)
                .map_err(|e| Error::Zlib(e.to_string()))?;
            encoder.finish().map_err(|e| Error::Zlib(e.to_string()))?;
        }
        fs::rename(&tmp_path, &path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o444));
        }

        Ok(oid)
    }

    /// Write an already-serialized object (header + data) to the loose store.
    ///
    /// Useful when the caller has the full store bytes (e.g. from stdin with
    /// `--literally`).
    ///
    /// # Errors
    ///
    /// - [`Error::CorruptObject`] — the provided bytes don't form a valid header.
    /// - [`Error::Io`] / [`Error::Zlib`] — storage errors.
    pub fn write_raw(&self, store_bytes: &[u8]) -> Result<ObjectId> {
        // Validate the header before storing
        parse_object_bytes(store_bytes)?;

        let oid = hash_bytes_with(self.hash_algo(), store_bytes);
        let path = self.object_path(&oid);
        if path.exists() {
            let _ = self.freshen_object(&oid);
            return Ok(oid);
        }
        if self.exists(&oid) {
            let _ = self.freshen_object(&oid);
            return Ok(oid);
        }

        let prefix_dir = path
            .parent()
            .ok_or_else(|| Error::PathError("object path has no parent".to_owned()))?;
        fs::create_dir_all(prefix_dir)?;

        let tmp_path = prefix_dir.join(format!("tmp_{}", oid.loose_suffix()));
        {
            let tmp_file = fs::File::create(&tmp_path)?;
            let mut encoder = ZlibEncoder::new(tmp_file, self.loose_compression());
            encoder
                .write_all(store_bytes)
                .map_err(|e| Error::Zlib(e.to_string()))?;
            encoder.finish().map_err(|e| Error::Zlib(e.to_string()))?;
        }
        fs::rename(&tmp_path, &path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o444));
        }

        Ok(oid)
    }

    /// Like [`Self::write_raw`] but only consults this object directory, not alternates.
    ///
    /// See [`Self::write_local`].
    ///
    /// # Errors
    ///
    /// Same as [`Self::write_raw`].
    pub fn write_raw_local(&self, store_bytes: &[u8]) -> Result<ObjectId> {
        parse_object_bytes(store_bytes)?;

        let oid = hash_bytes_with(self.hash_algo(), store_bytes);
        let path = self.object_path(&oid);
        if path.exists() {
            let _ = self.freshen_object(&oid);
            return Ok(oid);
        }
        if exists_materialized_in_objects_dir(&self.objects_dir, &oid) {
            let _ = self.freshen_object(&oid);
            return Ok(oid);
        }

        let prefix_dir = path
            .parent()
            .ok_or_else(|| Error::PathError("object path has no parent".to_owned()))?;
        fs::create_dir_all(prefix_dir)?;

        let tmp_path = prefix_dir.join(format!("tmp_{}", oid.loose_suffix()));
        {
            let tmp_file = fs::File::create(&tmp_path)?;
            let mut encoder = ZlibEncoder::new(tmp_file, self.loose_compression());
            encoder
                .write_all(store_bytes)
                .map_err(|e| Error::Zlib(e.to_string()))?;
            encoder.finish().map_err(|e| Error::Zlib(e.to_string()))?;
        }
        fs::rename(&tmp_path, &path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o444));
        }

        Ok(oid)
    }

    /// Returns true when a loose object exists at `oid`'s path and zlib-decompresses to a
    /// structurally valid `<type> <size>\0<payload>` object (type may be non-standard).
    ///
    /// Used for `git cat-file -e`, which succeeds for hand-crafted loose objects that
    /// [`Self::read`] rejects due to [`Error::UnknownObjectType`].
    #[must_use]
    pub fn loose_object_plumbing_ok(&self, oid: &ObjectId) -> bool {
        let path = self.object_path(oid);
        let Ok(file) = fs::File::open(&path) else {
            return false;
        };
        let Ok(raw) = read_zlib_loose_payload(file) else {
            return false;
        };
        loose_store_bytes_header_valid(&raw)
    }
}

fn loose_store_bytes_header_valid(raw: &[u8]) -> bool {
    let nul = match raw.iter().position(|&b| b == 0) {
        Some(i) => i,
        None => return false,
    };
    let header = &raw[..nul];
    let data = &raw[nul + 1..];
    let sp = match header.iter().position(|&b| b == b' ') {
        Some(i) => i,
        None => return false,
    };
    if sp == 0 || sp > 32 {
        return false;
    }
    let size_str = match std::str::from_utf8(&header[sp + 1..]) {
        Ok(s) => s,
        Err(_) => return false,
    };
    let size: usize = match size_str.parse() {
        Ok(s) => s,
        Err(_) => return false,
    };
    data.len() == size
}

/// Update `path`'s mtime to "now" (Git `utime(path, NULL)`), returning whether it succeeded.
fn touch_path_mtime(path: &Path) -> bool {
    // `utime(path, NULL)` sets both atime and mtime to the current time.
    let now = filetime::FileTime::now();
    filetime::set_file_times(path, now, now).is_ok()
}

fn freshen_object_in_objects_dir(objects_dir: &Path, oid: &ObjectId) -> bool {
    let Ok(indexes) = pack::read_local_pack_indexes_cached(objects_dir) else {
        return false;
    };
    for idx in &indexes {
        if idx.contains(oid) {
            let touched = touch_path_mtime(&idx.pack_path);
            if touched {
                // Keep the cached pack bytes valid: this mtime bump is ours, not a content change.
                pack::refresh_pack_bytes_signature(&idx.pack_path);
            }
            return touched;
        }
    }
    false
}

/// Hash the canonical store bytes of an object (`"<kind> <len>\0<data>"`) with
/// the given hash algorithm.
fn hash_object_data_with(algo: HashAlgo, kind: ObjectKind, data: &[u8]) -> ObjectId {
    let header = format!("{} {}\0", kind, data.len());
    match algo {
        HashAlgo::Sha1 => {
            let mut hasher = Sha1::new();
            hasher.update(header.as_bytes());
            hasher.update(data);
            ObjectId::from_bytes(hasher.finalize().as_slice())
                .unwrap_or_else(|_| unreachable!("SHA-1 is 20 bytes"))
        }
        HashAlgo::Sha256 => {
            let mut hasher = Sha256::new();
            hasher.update(header.as_bytes());
            hasher.update(data);
            ObjectId::from_bytes(hasher.finalize().as_slice())
                .unwrap_or_else(|_| unreachable!("SHA-256 is 32 bytes"))
        }
    }
}

/// Compute the digest of pre-built store bytes with the given hash algorithm.
fn hash_bytes_with(algo: HashAlgo, data: &[u8]) -> ObjectId {
    match algo {
        HashAlgo::Sha1 => {
            let mut hasher = Sha1::new();
            hasher.update(data);
            ObjectId::from_bytes(hasher.finalize().as_slice())
                .unwrap_or_else(|_| unreachable!("SHA-1 is 20 bytes"))
        }
        HashAlgo::Sha256 => {
            let mut hasher = Sha256::new();
            hasher.update(data);
            ObjectId::from_bytes(hasher.finalize().as_slice())
                .unwrap_or_else(|_| unreachable!("SHA-256 is 32 bytes"))
        }
    }
}

/// Build the canonical store byte sequence: `"<kind> <len>\0<data>"`.
fn build_store_bytes(kind: ObjectKind, data: &[u8]) -> Vec<u8> {
    let header = format!("{} {}\0", kind, data.len());
    let mut out = Vec::with_capacity(header.len() + data.len());
    out.extend_from_slice(header.as_bytes());
    out.extend_from_slice(data);
    out
}

/// Parse decompressed object bytes (`"<type> <size>\0<data>"`) into an [`Object`].
pub(crate) fn parse_object_bytes(raw: &[u8]) -> Result<Object> {
    parse_object_bytes_inner(raw, None)
}

pub(crate) fn parse_object_bytes_with_oid(raw: &[u8], oid: &ObjectId) -> Result<Object> {
    parse_object_bytes_inner(raw, Some(oid))
}

fn parse_object_bytes_inner(raw: &[u8], oid_hint: Option<&ObjectId>) -> Result<Object> {
    let nul = raw
        .iter()
        .position(|&b| b == 0)
        .ok_or_else(|| Error::CorruptObject("missing NUL in object header".to_owned()))?;

    let header = &raw[..nul];
    let data = raw[nul + 1..].to_vec();

    let sp = header
        .iter()
        .position(|&b| b == b' ')
        .ok_or_else(|| Error::CorruptObject("missing space in object header".to_owned()))?;

    if sp > 32 {
        let oid_str = oid_hint
            .map(|o| o.to_hex())
            .unwrap_or_else(|| hash_bytes_with(HashAlgo::Sha1, raw).to_hex());
        return Err(Error::ObjectHeaderTooLong { oid: oid_str });
    }

    let kind = ObjectKind::from_bytes(&header[..sp])?;

    let size_str = std::str::from_utf8(&header[sp + 1..])
        .map_err(|_| Error::CorruptObject("non-UTF-8 object size".to_owned()))?;
    let size: usize = size_str
        .parse()
        .map_err(|_| Error::CorruptObject(format!("invalid object size: {size_str}")))?;

    if data.len() != size {
        return Err(Error::CorruptObject(format!(
            "object size mismatch: header says {size} but got {}",
            data.len()
        )));
    }

    Ok(Object::new(kind, data))
}

/// Parse `GIT_ALTERNATE_OBJECT_DIRECTORIES` into a list of paths.
///
/// The env var contains colon-separated (`:`-separated on Unix) paths
/// to additional object directories to search. Supports double-quoted
/// entries with octal escapes (e.g. `\057` for `/`).
///
/// Relative paths are resolved against `resolve_base` (typically the work tree root).
fn env_alternate_dirs(resolve_base: Option<&Path>) -> Vec<PathBuf> {
    match std::env::var("GIT_ALTERNATE_OBJECT_DIRECTORIES") {
        Ok(val) if !val.is_empty() => {
            let mut dirs = parse_alternate_env(&val);
            if let Some(base) = resolve_base {
                for dir in &mut dirs {
                    if dir.is_relative() {
                        *dir = base.join(&dir);
                    }
                }
            }
            dirs
        }
        _ => Vec::new(),
    }
}

/// Parse a colon-separated alternates string, handling double-quoted entries
/// with octal escape sequences.
fn parse_alternate_env(val: &str) -> Vec<PathBuf> {
    let mut result = Vec::new();
    let mut chars = val.chars().peekable();
    while chars.peek().is_some() {
        if chars.peek() == Some(&':') {
            chars.next();
            continue;
        }
        if chars.peek() == Some(&'"') {
            // Try quoted parsing; if EOF is hit without closing quote,
            // fall back to treating the whole segment as a raw path.
            chars.next(); // consume the opening '"'
            let saved: Vec<char> = chars.clone().collect();
            let mut path = String::new();
            let mut properly_closed = false;
            loop {
                match chars.next() {
                    None => break,
                    Some('"') => {
                        properly_closed = true;
                        break;
                    }
                    Some('\\') => match chars.peek() {
                        Some(c) if c.is_ascii_digit() => {
                            let mut oct = String::new();
                            for _ in 0..3 {
                                if let Some(&c) = chars.peek() {
                                    if c.is_ascii_digit() {
                                        oct.push(c);
                                        chars.next();
                                    } else {
                                        break;
                                    }
                                } else {
                                    break;
                                }
                            }
                            if let Ok(byte) = u8::from_str_radix(&oct, 8) {
                                path.push(byte as char);
                            }
                        }
                        Some(_) => {
                            if let Some(c) = chars.next() {
                                match c {
                                    'n' => path.push('\n'),
                                    't' => path.push('\t'),
                                    'r' => path.push('\r'),
                                    _ => path.push(c),
                                }
                            }
                        }
                        None => {}
                    },
                    Some(c) => path.push(c),
                }
            }
            if !properly_closed {
                // Broken quoting: fall back to treating raw value (with leading ")
                // as a literal path.
                let raw: String = std::iter::once('"').chain(saved).collect();
                // Extract up to ':' or end
                let raw_path = raw.split(':').next().unwrap_or(&raw);
                if !raw_path.is_empty() {
                    result.push(PathBuf::from(raw_path));
                }
                // Advance past the ':' in the original chars (we consumed the saved copy)
                // Since chars is now at EOF, we need to handle remaining items.
                // Actually, we consumed chars fully. Let's reconstruct from raw.
                let remainder = &raw[raw_path.len()..];
                if let Some(rest) = remainder.strip_prefix(':') {
                    // Parse remaining entries
                    result.extend(parse_alternate_env(rest));
                }
                return result;
            } else if !path.is_empty() {
                result.push(PathBuf::from(path));
            }
        } else {
            let mut path = String::new();
            while let Some(&c) = chars.peek() {
                if c == ':' {
                    break;
                }
                path.push(c);
                chars.next();
            }
            if !path.is_empty() {
                result.push(PathBuf::from(path));
            }
        }
    }
    result
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;
    use tempfile::TempDir;

    #[test]
    fn round_trip_blob() {
        let dir = TempDir::new().unwrap();
        let odb = Odb::new(dir.path());
        let data = b"hello world";
        let oid = odb.write(ObjectKind::Blob, data).unwrap();
        let obj = odb.read(&oid).unwrap();
        assert_eq!(obj.kind, ObjectKind::Blob);
        assert_eq!(obj.data, data);
    }

    #[test]
    fn known_blob_hash() {
        // Verified: echo -n "hello" | git hash-object --stdin
        //        => b6fc4c620b67d95f953a5c1c1230aaab5db5a1b0
        let oid = Odb::hash_object_data(ObjectKind::Blob, b"hello");
        assert_eq!(oid.to_hex(), "b6fc4c620b67d95f953a5c1c1230aaab5db5a1b0");
    }
}
