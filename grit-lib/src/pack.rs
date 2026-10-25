//! Pack and pack-index helpers for object counting and verification.
//!
//! This module implements a focused subset of pack functionality required by
//! `count-objects` and `verify-pack`.

pub use crate::pack_index::{
    compute_fanout_from_entries, compute_fanout_from_oid_slices, pack_index_entry_matches_sha1_oid,
    parse_pack_index_bytes, PackIndex, PackIndexEntry, PackIndexEntryRef,
};

use crate::error::{Error, Result};
use crate::hash::{hash_object, verify_trailer};
use crate::objects::{HashAlgo, Object, ObjectId, ObjectInfo, ObjectKind};
pub use crate::pack_map::PackData;
use crate::unpack_objects::apply_delta_into;
use crate::zlib_inflate::ZlibInflateScratch;
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

/// Basic information about local packs.
#[derive(Debug, Clone, Default)]
pub struct LocalPackInfo {
    /// Number of valid local packs.
    pub pack_count: usize,
    /// Total objects across all valid local packs.
    pub object_count: usize,
    /// Combined on-disk bytes of `.pack` + `.idx`.
    pub size_bytes: u64,
    /// Set of all object IDs present in local packs.
    pub object_ids: HashSet<ObjectId>,
}

/// Read all valid `.idx` files in `objects/pack`.
///
/// # Errors
///
/// Returns [`Error::Io`] for directory-level failures. Individual invalid pack
/// pairs are skipped.
pub fn read_local_pack_indexes(objects_dir: &Path) -> Result<Vec<PackIndex>> {
    let pack_dir = objects_dir.join("pack");
    let rd = match fs::read_dir(&pack_dir) {
        Ok(rd) => rd,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(Error::Io(err)),
    };

    let mut out = Vec::new();
    for entry in rd {
        let entry = entry.map_err(Error::Io)?;
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("idx") {
            continue;
        }
        if let Ok(idx) = read_pack_index(&path) {
            // Ignore orphan `.idx` files (no `.pack`). They must not make `fsck` think objects
            // exist (`t7700-repack`); repack also skips them so a stray index does not block work.
            if !idx.pack_path.is_file() {
                continue;
            }
            out.push(idx);
        }
    }
    Ok(out)
}

/// Process-wide cache of parsed pack indexes and pack file bytes.
///
/// Hot lookups serve cached directory listings, parsed `.idx` files, and `.pack` bytes
/// without `stat` per access (Git prepares the pack list once and only re-scans on a
/// lookup miss when the pack directory's mtime changed). Pack bytes are re-read only on
/// cache miss or when parsing a pack object fails and the on-disk signature changed.
///
/// SHA-1 verification of the index trailer is **not** performed on cached reads: Git only
/// verifies pack indexes during `fsck`/`verify-pack`, not on every object lookup. Use
/// [`read_pack_index`] when verification is required.
pub(crate) mod pack_cache {
    use super::{read_pack_index_no_verify, Error, ObjectKind, PackIndex, Result};
    use crate::pack_map::{fingerprint_from_file, PackData, PackFingerprint};
    use std::collections::{HashMap, HashSet};
    use std::fs;
    use std::hash::{Hash, Hasher};
    use std::io;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::SystemTime;

    const DELTA_LRU_SHARD_COUNT: usize = 32;

    type ThreadLocalDirListing = Option<(PathBuf, SystemTime, Vec<Arc<PackIndex>>)>;

    // hygiene: per-thread pack read hot cache (indexes, bytes, dir listing); cleared on invalidate
    thread_local! {
        static PACK_DIR_INDEXES: std::cell::RefCell<ThreadLocalDirListing> =
            const { std::cell::RefCell::new(None) };
        static PACK_HIT_INDEX: std::cell::RefCell<Option<(PathBuf, Arc<PackIndex>)>> =
            const { std::cell::RefCell::new(None) };
        static PACK_BYTES: std::cell::RefCell<HashMap<PathBuf, Arc<PackData>>> =
            std::cell::RefCell::new(HashMap::new());
        static PACK_INDEXES: std::cell::RefCell<HashMap<PathBuf, Arc<PackIndex>>> =
            std::cell::RefCell::new(HashMap::new());
        static PACK_IDS: std::cell::RefCell<HashMap<PathBuf, u32>> =
            std::cell::RefCell::new(HashMap::new());
        static PACK_COUNT_VALIDATED: std::cell::RefCell<HashSet<PathBuf>> =
            std::cell::RefCell::new(HashSet::new());
        static PACK_DIR_LISTING_TRUSTED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
        /// [`PackStore::cache_epoch`] last seen on this thread (cross-thread invalidation).
        static THREAD_WARM_EPOCH: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
        /// [`PackStore`] address last warmed on this thread (distinct stores can share epoch 0).
        static THREAD_WARM_STORE: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
        static PACK_MONOTONIC_CURSOR: std::cell::RefCell<Option<(PathBuf, usize)>> =
            const { std::cell::RefCell::new(None) };
        static SINGLE_READER_MEMO: std::cell::RefCell<Option<SingleReaderMemo>> =
            const { std::cell::RefCell::new(None) };
    }

    /// The pack a [`super::SingleLocalPackReader`] last opened on this thread.
    ///
    /// Lets repeated opens (one per object read) skip re-deriving the same pack through the
    /// path-keyed caches and the shared catalog.
    struct SingleReaderMemo {
        objects_dir: PathBuf,
        pack_dir: PathBuf,
        idx: Arc<PackIndex>,
        pack_bytes: Arc<PackData>,
        pack_id: u32,
    }

    fn forget_single_reader_memo() {
        SINGLE_READER_MEMO.with(|cell| *cell.borrow_mut() = None);
    }

    /// Record the single local pack just opened for `objects_dir`.
    pub(crate) fn remember_single_reader(
        objects_dir: &Path,
        idx: &Arc<PackIndex>,
        pack_bytes: &Arc<PackData>,
        pack_id: u32,
    ) {
        SINGLE_READER_MEMO.with(|cell| {
            *cell.borrow_mut() = Some(SingleReaderMemo {
                objects_dir: objects_dir.to_path_buf(),
                pack_dir: objects_dir.join("pack"),
                idx: Arc::clone(idx),
                pack_bytes: Arc::clone(pack_bytes),
                pack_id,
            });
        });
    }

    /// The single local pack previously opened for `objects_dir`, if this thread's trusted
    /// pack listing still consists of exactly that pack.
    ///
    /// Returns `None` whenever the listing is untrusted (so the caller rechecks the pack
    /// directory) or has changed since the pack was remembered.
    pub(crate) fn single_reader_for(
        objects_dir: &Path,
    ) -> Option<(Arc<PackIndex>, Arc<PackData>, u32)> {
        if !PACK_DIR_LISTING_TRUSTED.get() {
            return None;
        }
        SINGLE_READER_MEMO.with(|memo| {
            let memo = memo.borrow();
            let memo = memo.as_ref()?;
            if memo.objects_dir.as_os_str() != objects_dir.as_os_str() {
                return None;
            }
            let listing_unchanged = PACK_DIR_INDEXES.with(|cell| {
                cell.borrow().as_ref().is_some_and(|(dir, _, indexes)| {
                    dir.as_os_str() == memo.pack_dir.as_os_str()
                        && indexes.len() == 1
                        && Arc::ptr_eq(&indexes[0], &memo.idx)
                })
            });
            if !listing_unchanged {
                return None;
            }
            let hit_current = PACK_HIT_INDEX.with(|cell| {
                cell.borrow().as_ref().is_some_and(|(dir, idx)| {
                    dir.as_os_str() == objects_dir.as_os_str() && Arc::ptr_eq(idx, &memo.idx)
                })
            });
            if !hit_current {
                return None;
            }
            Some((
                Arc::clone(&memo.idx),
                Arc::clone(&memo.pack_bytes),
                memo.pack_id,
            ))
        })
    }

    fn store_thread_local_key(store: &crate::pack_store::PackStore) -> usize {
        store as *const crate::pack_store::PackStore as usize
    }

    pub(crate) fn clear_thread_local_dir_indexes() {
        PACK_DIR_INDEXES.with(|cell| *cell.borrow_mut() = None);
        PACK_HIT_INDEX.with(|cell| *cell.borrow_mut() = None);
        PACK_BYTES.with(|cell| cell.borrow_mut().clear());
        PACK_INDEXES.with(|cell| cell.borrow_mut().clear());
        PACK_IDS.with(|cell| cell.borrow_mut().clear());
        PACK_COUNT_VALIDATED.with(|cell| cell.borrow_mut().clear());
        PACK_DIR_LISTING_TRUSTED.set(false);
        THREAD_WARM_EPOCH.set(0);
        THREAD_WARM_STORE.set(0);
        super::clear_pack_read_info_cache();
        PACK_MONOTONIC_CURSOR.with(|cell| *cell.borrow_mut() = None);
        forget_single_reader_memo();
    }

    pub(crate) fn try_read_object_via_single_pack_reader(
        objects_dir: &Path,
        oid: &crate::objects::ObjectId,
    ) -> Result<Option<crate::objects::Object>> {
        if !crate::pack_store::PackStore::read_context_matches_objects_dir(objects_dir) {
            return Ok(None);
        }
        let Some(mut reader) = super::SingleLocalPackReader::open_allow_loose(objects_dir)? else {
            return Ok(None);
        };
        match reader.read_object(oid) {
            Ok(obj) => Ok(Some(obj)),
            Err(Error::ObjectNotFound(_)) => Ok(None),
            Err(err) => Err(err),
        }
    }

    pub(crate) fn try_read_object_via_single_pack_reader_into(
        objects_dir: &Path,
        oid: &crate::objects::ObjectId,
        buf: &mut Vec<u8>,
    ) -> Result<Option<crate::objects::ObjectKind>> {
        if !crate::pack_store::PackStore::read_context_matches_objects_dir(objects_dir) {
            return Ok(None);
        }
        let Some(mut reader) = super::SingleLocalPackReader::open_allow_loose(objects_dir)? else {
            return Ok(None);
        };
        match reader.read_object_into(oid, buf) {
            Ok(kind) => Ok(Some(kind)),
            Err(Error::ObjectNotFound(_)) => Ok(None),
            Err(err) => Err(err),
        }
    }

    /// Drop thread-local pack caches after invalidation on this or another thread.
    pub(crate) fn clear_thread_local_after_invalidation(new_epoch: u64) {
        clear_thread_local_dir_indexes();
        THREAD_WARM_EPOCH.set(new_epoch);
    }

    pub(crate) fn sync_thread_local_with_store(store: &crate::pack_store::PackStore) {
        if !crate::pack_store::PackStore::has_context() {
            return;
        }
        let epoch = store.cache_epoch();
        let store_key = store_thread_local_key(store);
        if THREAD_WARM_EPOCH.get() == epoch && THREAD_WARM_STORE.get() == store_key {
            return;
        }
        clear_thread_local_dir_indexes();
        THREAD_WARM_EPOCH.set(epoch);
        THREAD_WARM_STORE.set(store_key);
    }

    fn drop_thread_local_dir_listing(pack_dir: &Path) {
        PACK_DIR_INDEXES.with(|cell| {
            if cell
                .borrow()
                .as_ref()
                .is_some_and(|(dir, _, _)| dir == pack_dir)
            {
                *cell.borrow_mut() = None;
                PACK_DIR_LISTING_TRUSTED.set(false);
            }
        });
    }

    fn drop_thread_local_pack_bytes(pack_path: &Path) {
        PACK_BYTES.with(|cell| {
            cell.borrow_mut().remove(pack_path);
        });
        forget_single_reader_memo();
    }

    /// Drop thread-local indexes, hints, and directory listing tied to a replaced pack path.
    fn invalidate_thread_local_for_pack_path_change(pack_path: &Path) {
        let idx_path = pack_path.with_extension("idx");
        drop_thread_local_pack_bytes(pack_path);
        PACK_INDEXES.with(|cell| {
            cell.borrow_mut().remove(&idx_path);
        });
        PACK_IDS.with(|cell| {
            cell.borrow_mut().remove(pack_path);
        });
        PACK_COUNT_VALIDATED.with(|cell| {
            cell.borrow_mut().remove(pack_path);
        });
        PACK_HIT_INDEX.with(|cell| {
            let drop_hint = cell
                .borrow()
                .as_ref()
                .is_some_and(|(_, idx)| idx.pack_path == pack_path || idx.idx_path == idx_path);
            if drop_hint {
                *cell.borrow_mut() = None;
            }
        });
        if let Some(pack_dir) = pack_path.parent() {
            drop_thread_local_dir_listing(pack_dir);
        }
        super::clear_pack_read_info_cache();
    }

    /// Seed thread-local pack caches from `store` (one catalog read lock; hot reads avoid the catalog).
    pub(crate) fn warm_thread_local_from_store(store: &crate::pack_store::PackStore) {
        if !crate::pack_store::PackStore::has_context() {
            return;
        }
        let epoch = store.cache_epoch();
        let store_key = store_thread_local_key(store);
        if THREAD_WARM_EPOCH.get() == epoch
            && THREAD_WARM_STORE.get() == store_key
            && PACK_DIR_LISTING_TRUSTED.get()
        {
            return;
        }
        PACK_BYTES.with(|cell| cell.borrow_mut().clear());
        PACK_INDEXES.with(|cell| cell.borrow_mut().clear());
        PACK_IDS.with(|cell| cell.borrow_mut().clear());
        PACK_COUNT_VALIDATED.with(|cell| cell.borrow_mut().clear());
        PACK_HIT_INDEX.with(|cell| *cell.borrow_mut() = None);
        forget_single_reader_memo();
        super::clear_pack_read_info_cache();
        let objects_dir = store.objects_dir();
        let pack_dir = objects_dir.join("pack");
        store.with_catalog_read(|g| {
            if let Some(cached) = g.by_dir.get(&pack_dir) {
                PACK_DIR_INDEXES.with(|cell| {
                    *cell.borrow_mut() =
                        Some((pack_dir.clone(), cached.dir_mtime, cached.indexes.clone()));
                });
            }
            for (path, entry) in &g.by_pack {
                remember_pack_bytes(path, &entry.bytes);
            }
            for (path, entry) in &g.by_idx {
                remember_index(path, &entry.idx);
            }
            PACK_IDS.with(|cell| {
                cell.borrow_mut()
                    .extend(g.pack_ids.iter().map(|(k, v)| (k.clone(), *v)));
            });
            PACK_COUNT_VALIDATED.with(|cell| {
                cell.borrow_mut()
                    .extend(g.validated_pack_counts.iter().cloned());
            });
            PACK_DIR_LISTING_TRUSTED.set(true);
            THREAD_WARM_EPOCH.set(store.cache_epoch());
            THREAD_WARM_STORE.set(store_thread_local_key(store));
        });
    }

    fn cached_pack_bytes(pack_path: &Path) -> Option<Arc<PackData>> {
        if !crate::pack_store::PackStore::has_context() {
            return None;
        }
        PACK_BYTES.with(|cell| cell.borrow().get(pack_path).map(Arc::clone))
    }

    fn remember_pack_bytes(pack_path: &Path, bytes: &Arc<PackData>) {
        if !crate::pack_store::PackStore::has_context() {
            return;
        }
        PACK_BYTES.with(|cell| {
            cell.borrow_mut()
                .insert(pack_path.to_path_buf(), Arc::clone(bytes));
        });
        forget_single_reader_memo();
    }

    pub(crate) fn remember_pack_hit(objects_dir: &Path, idx: &Arc<PackIndex>) {
        if !crate::pack_store::PackStore::read_context_matches_objects_dir(objects_dir) {
            return;
        }
        promote_pack_index(objects_dir, &idx.idx_path);
        PACK_HIT_INDEX.with(|cell| {
            *cell.borrow_mut() = Some((objects_dir.to_path_buf(), Arc::clone(idx)));
        });
    }

    pub(crate) fn find_offset_in_pack_read_context(
        objects_dir: &Path,
        idx: &PackIndex,
        oid: &crate::objects::ObjectId,
    ) -> Result<Option<u64>> {
        if !crate::pack_store::PackStore::read_context_matches_objects_dir(objects_dir) {
            return idx.find_offset(oid);
        }
        PACK_MONOTONIC_CURSOR.with(|cell| {
            let mut slot = cell.borrow_mut();
            if slot.as_ref().is_none_or(|(dir, _)| dir != objects_dir) {
                *slot = Some((objects_dir.to_path_buf(), 0));
            }
            let Some((_, cursor)) = slot.as_mut() else {
                return idx.find_offset(oid);
            };
            idx.find_offset_monotonic(oid, cursor)
        })
    }

    pub(crate) fn try_read_from_hinted_pack(
        objects_dir: &Path,
        oid: &crate::objects::ObjectId,
    ) -> Result<Option<crate::objects::Object>> {
        if !crate::pack_store::PackStore::read_context_matches_objects_dir(objects_dir) {
            return Ok(None);
        }
        let idx = PACK_HIT_INDEX.with(|cell| {
            cell.borrow()
                .as_ref()
                .and_then(|(dir, idx)| (dir == objects_dir).then(|| Arc::clone(idx)))
        });
        let Some(idx) = idx else {
            return Ok(None);
        };
        let Some(offset) = find_offset_in_pack_read_context(objects_dir, &idx, oid)? else {
            return Ok(None);
        };
        match super::read_object_from_pack_at_offset(&idx, offset) {
            Ok(obj) => Ok(Some(obj)),
            Err(Error::ObjectNotFound(_)) => Ok(None),
            Err(err) => Err(err),
        }
    }

    pub(crate) fn try_read_from_hinted_pack_into(
        objects_dir: &Path,
        oid: &crate::objects::ObjectId,
        buf: &mut Vec<u8>,
    ) -> Result<Option<crate::objects::ObjectKind>> {
        if !crate::pack_store::PackStore::read_context_matches_objects_dir(objects_dir) {
            return Ok(None);
        }
        let idx = PACK_HIT_INDEX.with(|cell| {
            cell.borrow()
                .as_ref()
                .and_then(|(dir, idx)| (dir == objects_dir).then(|| Arc::clone(idx)))
        });
        let Some(idx) = idx else {
            return Ok(None);
        };
        let Some(offset) = find_offset_in_pack_read_context(objects_dir, &idx, oid)? else {
            return Ok(None);
        };
        match super::read_object_at_depth_into(&idx, offset, 0, false, buf) {
            Ok(kind) => Ok(Some(kind)),
            Err(Error::ObjectNotFound(_)) => Ok(None),
            Err(err) => Err(err),
        }
    }

    pub(crate) fn try_read_info_from_hinted_pack(
        objects_dir: &Path,
        oid: &crate::objects::ObjectId,
    ) -> Result<Option<crate::objects::ObjectInfo>> {
        if !crate::pack_store::PackStore::read_context_matches_objects_dir(objects_dir) {
            return Ok(None);
        }
        let idx = PACK_HIT_INDEX.with(|cell| {
            cell.borrow()
                .as_ref()
                .and_then(|(dir, idx)| (dir == objects_dir).then(|| Arc::clone(idx)))
        });
        let Some(idx) = idx else {
            return Ok(None);
        };
        let Some(offset) = idx.find_offset(oid)? else {
            return Ok(None);
        };
        match super::read_object_info_from_pack_at_offset(&idx, offset) {
            Ok(info) => Ok(Some(info)),
            Err(Error::ObjectNotFound(_)) => Ok(None),
            Err(err) => Err(err),
        }
    }
    fn cached_dir_indexes(objects_dir: &Path, pack_dir: &Path) -> Option<Vec<Arc<PackIndex>>> {
        if !crate::pack_store::PackStore::read_context_matches_objects_dir(objects_dir) {
            return None;
        }
        let trust_listing = PACK_DIR_LISTING_TRUSTED.get();
        let dir_mt = if trust_listing {
            None
        } else {
            Some(dir_mtime(pack_dir))
        };
        PACK_DIR_INDEXES.with(|cell| {
            cell.borrow()
                .as_ref()
                .and_then(|(dir, cached_mt, indexes)| {
                    if dir != pack_dir {
                        return None;
                    }
                    if trust_listing || dir_mt == Some(*cached_mt) {
                        Some(indexes.clone())
                    } else {
                        None
                    }
                })
        })
    }

    fn remember_dir_indexes(objects_dir: &Path, pack_dir: &Path, indexes: &[Arc<PackIndex>]) {
        if !crate::pack_store::PackStore::read_context_matches_objects_dir(objects_dir) {
            return;
        }
        let dir_mt = dir_mtime(pack_dir);
        PACK_DIR_INDEXES.with(|cell| {
            *cell.borrow_mut() = Some((pack_dir.to_path_buf(), dir_mt, indexes.to_vec()));
        });
    }

    struct CachedDir {
        dir_mtime: SystemTime,
        indexes: Vec<Arc<PackIndex>>,
        midx_mtime: SystemTime,
        midx_pack_names: HashSet<String>,
    }

    struct CachedIdx {
        idx: Arc<PackIndex>,
    }

    struct CachedPack {
        mtime: SystemTime,
        size: u64,
        bytes: Arc<PackData>,
    }

    /// Default retained delta-base bytes (`core.deltaBaseCacheLimit`).
    pub(super) const DELTA_BASE_CACHE_DEFAULT: usize = 96 * 1024 * 1024;

    #[derive(Clone, Copy, PartialEq, Eq, Hash)]
    pub(crate) struct DeltaCacheKey {
        pack_id: u32,
        offset: u64,
    }

    /// Byte-bounded LRU of resolved **intermediate** delta bases (never the object being returned).
    struct DeltaBaseLru {
        byte_limit: usize,
        bytes_used: usize,
        entries: HashMap<DeltaCacheKey, (ObjectKind, Arc<[u8]>)>,
        head: Option<DeltaCacheKey>,
        tail: Option<DeltaCacheKey>,
        links: HashMap<DeltaCacheKey, (Option<DeltaCacheKey>, Option<DeltaCacheKey>)>,
    }

    impl Default for DeltaBaseLru {
        fn default() -> Self {
            Self {
                byte_limit: DELTA_BASE_CACHE_DEFAULT,
                bytes_used: 0,
                entries: HashMap::new(),
                head: None,
                tail: None,
                links: HashMap::new(),
            }
        }
    }

    impl DeltaBaseLru {
        fn set_byte_limit(&mut self, limit: usize) {
            if limit == 0 {
                self.clear();
                self.byte_limit = 0;
                return;
            }
            self.byte_limit = limit;
            while self.bytes_used > self.byte_limit {
                let Some(key) = self.head else {
                    break;
                };
                self.evict_key(key);
            }
        }

        fn unlink(&mut self, key: DeltaCacheKey) {
            let Some((prev, next)) = self.links.remove(&key) else {
                return;
            };
            match prev {
                Some(p) => {
                    if let Some(link) = self.links.get_mut(&p) {
                        link.1 = next;
                    }
                }
                None => self.head = next,
            }
            match next {
                Some(n) => {
                    if let Some(link) = self.links.get_mut(&n) {
                        link.0 = prev;
                    }
                }
                None => self.tail = prev,
            }
        }

        fn append_tail(&mut self, key: DeltaCacheKey) {
            let prev = self.tail;
            self.links.insert(key, (prev, None));
            if let Some(p) = prev {
                if let Some(link) = self.links.get_mut(&p) {
                    link.1 = Some(key);
                }
            } else {
                self.head = Some(key);
            }
            self.tail = Some(key);
        }

        fn touch(&mut self, key: DeltaCacheKey) {
            self.unlink(key);
            self.append_tail(key);
        }

        fn evict_key(&mut self, key: DeltaCacheKey) {
            self.unlink(key);
            if let Some((_, data)) = self.entries.remove(&key) {
                self.bytes_used = self.bytes_used.saturating_sub(data.len());
            }
        }

        fn get(&mut self, key: DeltaCacheKey) -> Option<(ObjectKind, Arc<[u8]>)> {
            let (kind, data) = self.entries.get(&key).map(|(k, d)| (*k, Arc::clone(d)))?;
            self.touch(key);
            Some((kind, data))
        }

        fn put(&mut self, key: DeltaCacheKey, kind: ObjectKind, data: Arc<[u8]>) {
            if self.byte_limit == 0 {
                return;
            }
            let sz = data.len();
            if sz > self.byte_limit {
                return;
            }
            use std::collections::hash_map::Entry;
            match self.entries.entry(key) {
                Entry::Occupied(mut slot) => {
                    let old_len = slot.get().1.len();
                    slot.insert((kind, Arc::clone(&data)));
                    self.bytes_used = self.bytes_used.saturating_sub(old_len).saturating_add(sz);
                    self.touch(key);
                }
                Entry::Vacant(slot) => {
                    slot.insert((kind, Arc::clone(&data)));
                    self.bytes_used = self.bytes_used.saturating_add(sz);
                    self.append_tail(key);
                }
            }
            while self.bytes_used > self.byte_limit {
                let Some(evict) = self.head else {
                    break;
                };
                self.evict_key(evict);
            }
        }

        fn drop_pack(&mut self, pack_id: u32) {
            let keys: Vec<DeltaCacheKey> = self
                .entries
                .keys()
                .copied()
                .filter(|k| k.pack_id == pack_id)
                .collect();
            for key in keys {
                self.evict_key(key);
            }
        }

        fn clear(&mut self) {
            self.entries.clear();
            self.links.clear();
            self.head = None;
            self.tail = None;
            self.bytes_used = 0;
        }
    }

    fn shard_index(key: DeltaCacheKey) -> usize {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        key.hash(&mut hasher);
        (hasher.finish() as usize) % DELTA_LRU_SHARD_COUNT
    }

    /// Delta-base LRU sharded by `(pack_id, offset)` so concurrent reads rarely contend.
    pub(crate) struct ShardedDeltaBaseLru {
        cache_enabled: AtomicBool,
        lru_populated: AtomicBool,
        total_byte_limit: AtomicUsize,
        shards: Box<[Mutex<DeltaBaseLru>; DELTA_LRU_SHARD_COUNT]>,
    }

    impl ShardedDeltaBaseLru {
        pub(crate) fn new() -> Self {
            let per_shard = DELTA_BASE_CACHE_DEFAULT / DELTA_LRU_SHARD_COUNT;
            Self {
                cache_enabled: AtomicBool::new(true),
                lru_populated: AtomicBool::new(false),
                total_byte_limit: AtomicUsize::new(DELTA_BASE_CACHE_DEFAULT),
                shards: Box::new(std::array::from_fn(|_| {
                    Mutex::new(DeltaBaseLru {
                        byte_limit: per_shard,
                        ..DeltaBaseLru::default()
                    })
                })),
            }
        }

        pub(crate) fn set_byte_limit(&self, limit: usize) {
            self.total_byte_limit.store(limit, Ordering::Release);
            if limit == 0 {
                self.cache_enabled.store(false, Ordering::Release);
                self.clear();
                return;
            }
            self.cache_enabled.store(true, Ordering::Release);
            let base = limit / DELTA_LRU_SHARD_COUNT;
            let rem = limit % DELTA_LRU_SHARD_COUNT;
            let mut any = false;
            for (i, shard) in self.shards.iter().enumerate() {
                let cap = base + usize::from(i < rem);
                let mut g = shard.lock().unwrap_or_else(|e| e.into_inner());
                g.set_byte_limit(cap);
                any |= !g.entries.is_empty();
            }
            self.lru_populated.store(any, Ordering::Release);
        }

        pub(crate) fn get(&self, key: DeltaCacheKey) -> Option<(ObjectKind, Arc<[u8]>)> {
            if !self.cache_enabled.load(Ordering::Acquire)
                || !self.lru_populated.load(Ordering::Acquire)
            {
                return None;
            }
            let shard = &self.shards[shard_index(key)];
            let mut g = shard.lock().unwrap_or_else(|e| e.into_inner());
            g.get(key)
        }

        pub(crate) fn put_many(&self, kind: ObjectKind, items: &[(u32, u64, Arc<[u8]>)]) {
            if items.is_empty() || !self.cache_enabled.load(Ordering::Acquire) {
                return;
            }
            for &(pack_id, offset, ref data) in items {
                let key = DeltaCacheKey { pack_id, offset };
                let shard = &self.shards[shard_index(key)];
                let mut g = shard.lock().unwrap_or_else(|e| e.into_inner());
                if g.byte_limit == 0 {
                    continue;
                }
                g.put(key, kind, Arc::clone(data));
            }
            self.lru_populated.store(true, Ordering::Release);
        }

        pub(crate) fn clear(&self) {
            for shard in self.shards.iter() {
                shard.lock().unwrap_or_else(|e| e.into_inner()).clear();
            }
            self.lru_populated.store(false, Ordering::Release);
        }

        pub(crate) fn drop_pack(&self, pack_id: u32) {
            let mut any = false;
            for shard in self.shards.iter() {
                let mut g = shard.lock().unwrap_or_else(|e| e.into_inner());
                g.drop_pack(pack_id);
                any |= !g.entries.is_empty();
            }
            self.lru_populated.store(any, Ordering::Release);
        }

        pub(crate) fn caching_enabled(&self) -> bool {
            self.cache_enabled.load(Ordering::Acquire)
        }

        #[cfg(test)]
        pub(crate) fn bytes_used(&self) -> usize {
            self.shards
                .iter()
                .map(|s| s.lock().unwrap_or_else(|e| e.into_inner()).bytes_used)
                .sum()
        }

        #[cfg(test)]
        pub(crate) fn contains(&self, pack_id: u32, offset: u64) -> bool {
            let key = DeltaCacheKey { pack_id, offset };
            self.shards[shard_index(key)]
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .entries
                .contains_key(&key)
        }
    }

    #[derive(Default)]
    pub(crate) struct CatalogState {
        by_dir: HashMap<PathBuf, CachedDir>,
        by_idx: HashMap<PathBuf, CachedIdx>,
        by_pack: HashMap<PathBuf, CachedPack>,
        /// Small id assigned when a pack file is first registered in this process.
        pack_ids: HashMap<PathBuf, u32>,
        next_pack_id: u32,
        /// Packs whose header object count was matched to the index (hot read path).
        validated_pack_counts: HashSet<PathBuf>,
        /// Per `objects/pack` directory rescan count (parallel-safe; tests only).
        #[cfg(test)]
        test_dir_rescan_counts: HashMap<PathBuf, u64>,
    }

    pub(crate) fn with_current<R>(f: impl FnOnce(&mut CatalogState) -> R) -> R {
        crate::pack_store::PackStore::with_current_or_implicit(f)
    }

    pub(crate) fn clear_catalog_on(m: &std::sync::RwLock<CatalogState>) {
        let mut g = m.write().unwrap_or_else(|e| e.into_inner());
        clear_inner(&mut g);
    }

    pub(crate) fn configure_delta_base_on(
        delta: &ShardedDeltaBaseLru,
        cfg: Option<&crate::config::ConfigSet>,
    ) {
        let bytes = cfg
            .and_then(|config| config.get("core.deltaBaseCacheLimit"))
            .as_ref()
            .and_then(|value| crate::config::parse_i64(value).ok())
            .and_then(|value| usize::try_from(value).ok())
            .unwrap_or(DELTA_BASE_CACHE_DEFAULT);
        delta.set_byte_limit(bytes);
    }

    #[cfg(test)]
    thread_local! {
        static TEST_MARKER_STAT_COUNT: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    }

    fn marker_sidecar_is_file(path: &Path) -> bool {
        #[cfg(test)]
        TEST_MARKER_STAT_COUNT.with(|count| count.set(count.get().saturating_add(1)));
        path.is_file()
    }

    /// Whether sibling `.promisor` / `.mtimes` markers exist next to `pack_path`.
    pub fn sidecar_flags(pack_path: &Path) -> (bool, bool) {
        (
            marker_sidecar_is_file(&pack_path.with_extension("promisor")),
            marker_sidecar_is_file(&pack_path.with_extension("mtimes")),
        )
    }

    fn refresh_pack_sidecar_flags(idx: Arc<PackIndex>) -> Arc<PackIndex> {
        let (is_promisor, is_cruft) = sidecar_flags(&idx.pack_path);
        if idx.is_promisor == is_promisor && idx.is_cruft == is_cruft {
            return idx;
        }
        let updated = Arc::new(idx.with_sidecar_flags(is_promisor, is_cruft));
        with_current(|g| {
            g.by_idx.insert(
                idx.idx_path.clone(),
                CachedIdx {
                    idx: Arc::clone(&updated),
                },
            );
        });
        updated
    }

    fn dir_mtime(path: &Path) -> SystemTime {
        fs::metadata(path)
            .and_then(|m| m.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH)
    }

    fn file_signature(path: &Path) -> Option<(SystemTime, u64)> {
        #[cfg(test)]
        crate::hot_path_test_metrics::record_pack_signature_stat_for_active_scope();
        let m = fs::metadata(path).ok()?;
        let mtime = m.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        Some((mtime, m.len()))
    }

    pub fn pack_object_count_validated(pack_path: &Path) -> bool {
        if crate::pack_store::PackStore::has_context()
            && PACK_COUNT_VALIDATED.with(|cell| cell.borrow().contains(pack_path))
        {
            return true;
        }
        crate::pack_store::PackStore::active_for_reads()
            .with_catalog_read(|g| g.validated_pack_counts.contains(pack_path))
    }

    pub fn mark_pack_object_count_validated(pack_path: &Path) {
        if crate::pack_store::PackStore::has_context() {
            PACK_COUNT_VALIDATED.with(|cell| {
                cell.borrow_mut().insert(pack_path.to_path_buf());
            });
        }
        let store = crate::pack_store::PackStore::active_for_reads();
        if store.with_catalog_read(|g| g.validated_pack_counts.contains(pack_path)) {
            return;
        }
        let _ = store.try_with_catalog(|g| {
            g.validated_pack_counts.insert(pack_path.to_path_buf());
        });
    }

    fn cached_index(idx_path: &Path) -> Option<Arc<PackIndex>> {
        if !crate::pack_store::PackStore::has_context() {
            return None;
        }
        PACK_INDEXES.with(|cell| cell.borrow().get(idx_path).map(Arc::clone))
    }

    fn remember_index(idx_path: &Path, idx: &Arc<PackIndex>) {
        if !crate::pack_store::PackStore::has_context() {
            return;
        }
        PACK_INDEXES.with(|cell| {
            cell.borrow_mut()
                .insert(idx_path.to_path_buf(), Arc::clone(idx));
        });
    }

    fn index_cache_hit_stale(idx: &PackIndex) -> bool {
        !idx.pack_path.is_file()
    }

    /// Get a parsed pack index from cache, parsing from disk only on cache miss.
    pub fn get_index(idx_path: &Path) -> Result<Arc<PackIndex>> {
        if let Some(hit) = cached_index(idx_path) {
            if index_cache_hit_stale(&hit) {
                PACK_INDEXES.with(|cell| {
                    cell.borrow_mut().remove(idx_path);
                });
            } else {
                return Ok(hit);
            }
        }
        let store = crate::pack_store::PackStore::active_for_reads();
        if let Some(hit) =
            store.with_catalog_read(|g| g.by_idx.get(idx_path).map(|c| Arc::clone(&c.idx)))
        {
            if index_cache_hit_stale(&hit) {
                store.with_catalog(|g| {
                    g.by_idx.remove(idx_path);
                });
            } else {
                remember_index(idx_path, &hit);
                return Ok(hit);
            }
        }
        crate::trace::trace_span!("grit.pack.index.open");
        let parsed = Arc::new(read_pack_index_no_verify(idx_path)?);
        store.with_catalog(|g| {
            g.by_idx.insert(
                idx_path.to_path_buf(),
                CachedIdx {
                    idx: Arc::clone(&parsed),
                },
            );
        });
        remember_index(idx_path, &parsed);
        Ok(parsed)
    }

    fn rescan_dir_indexes(objects_dir: &Path) -> Result<Vec<Arc<PackIndex>>> {
        let pack_dir = objects_dir.join("pack");
        drop_thread_local_dir_listing(&pack_dir);
        let dir_mt = dir_mtime(&pack_dir);

        let rd = match fs::read_dir(&pack_dir) {
            Ok(rd) => rd,
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                with_current(|g| {
                    g.by_dir.insert(
                        pack_dir.clone(),
                        CachedDir {
                            dir_mtime: dir_mt,
                            indexes: Vec::new(),
                            midx_mtime: SystemTime::UNIX_EPOCH,
                            midx_pack_names: HashSet::new(),
                        },
                    );
                });
                return Ok(Vec::new());
            }
            Err(err) => return Err(Error::Io(err)),
        };

        let mut out = Vec::new();
        for entry in rd {
            let entry = entry.map_err(Error::Io)?;
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("idx") {
                continue;
            }
            let Ok(idx) = get_index(&path) else {
                continue;
            };
            if !idx.pack_path.is_file() {
                continue;
            }
            out.push(refresh_pack_sidecar_flags(idx));
        }

        let midx_pack_names = load_midx_pack_names(&pack_dir);
        with_current(|g| {
            #[cfg(test)]
            {
                *g.test_dir_rescan_counts
                    .entry(pack_dir.clone())
                    .or_insert(0) += 1;
            }
            g.by_dir.insert(
                pack_dir,
                CachedDir {
                    dir_mtime: dir_mt,
                    indexes: out.clone(),
                    midx_mtime: midx_pack_names.0,
                    midx_pack_names: midx_pack_names.1,
                },
            );
        });
        Ok(out)
    }

    fn load_midx_pack_names(pack_dir: &Path) -> (SystemTime, HashSet<String>) {
        let Some(midx_path) = crate::midx::resolve_tip_midx_path(pack_dir) else {
            return (SystemTime::UNIX_EPOCH, HashSet::new());
        };
        let mtime = fs::metadata(&midx_path)
            .and_then(|m| m.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH);
        let objects_dir = pack_dir.parent().unwrap_or(pack_dir);
        let names = crate::midx::read_midx_pack_idx_names(objects_dir)
            .unwrap_or_default()
            .into_iter()
            .collect();
        (mtime, names)
    }

    fn refresh_midx_pack_names_locked(pack_dir: &Path, cached: &mut CachedDir) {
        let (mtime, names) = load_midx_pack_names(pack_dir);
        if cached.midx_mtime != mtime {
            cached.midx_mtime = mtime;
            cached.midx_pack_names = names;
        }
    }

    /// Move `idx_path` to the front of the MRU pack search order for `objects_dir`.
    pub fn promote_pack_index(objects_dir: &Path, idx_path: &Path) {
        let pack_dir = objects_dir.join("pack");
        let store = crate::pack_store::PackStore::active_for_reads();
        let _ = store.try_with_catalog(|g| {
            let Some(cached) = g.by_dir.get_mut(&pack_dir) else {
                return;
            };
            let Some(pos) = cached
                .indexes
                .iter()
                .position(|idx| idx.idx_path == idx_path)
            else {
                return;
            };
            if pos == 0 {
                return;
            }
            let hit = cached.indexes.remove(pos);
            cached.indexes.insert(0, hit);
        });
    }

    /// Pack index basenames listed in the active multi-pack-index, if any.
    pub fn midx_covered_pack_names(objects_dir: &Path) -> HashSet<String> {
        let pack_dir = objects_dir.join("pack");
        let cached = with_current(|g| {
            let cached = g.by_dir.get_mut(&pack_dir)?;
            refresh_midx_pack_names_locked(&pack_dir, cached);
            Some(cached.midx_pack_names.clone())
        });
        if let Some(names) = cached {
            return names;
        }
        load_midx_pack_names(&pack_dir).1
    }

    /// Get all `.idx` files for `objects_dir`, using the cached directory listing when present.
    pub fn get_dir_indexes(objects_dir: &Path) -> Result<Vec<Arc<PackIndex>>> {
        let pack_dir = objects_dir.join("pack");
        let store = crate::pack_store::PackStore::active_for_reads();
        if crate::pack_store::PackStore::has_context() {
            sync_thread_local_with_store(&store);
        }
        if let Some(hit) = cached_dir_indexes(objects_dir, &pack_dir) {
            return Ok(hit);
        }
        let dir_mt = dir_mtime(&pack_dir);
        if let Some(hit) = store.with_catalog_read(|g| {
            g.by_dir
                .get(&pack_dir)
                .and_then(|c| (c.dir_mtime == dir_mt).then(|| c.indexes.clone()))
        }) {
            remember_dir_indexes(objects_dir, &pack_dir, &hit);
            return Ok(hit);
        }
        let fresh = rescan_dir_indexes(objects_dir)?;
        remember_dir_indexes(objects_dir, &pack_dir, &fresh);
        Ok(fresh)
    }

    fn idx_paths_on_disk(pack_dir: &Path) -> Result<HashSet<PathBuf>> {
        let mut out = HashSet::new();
        let rd = match fs::read_dir(pack_dir) {
            Ok(rd) => rd,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(out),
            Err(err) => return Err(Error::Io(err)),
        };
        for entry in rd {
            let entry = entry.map_err(Error::Io)?;
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("idx") {
                continue;
            }
            if path.with_extension("pack").is_file() {
                out.insert(path);
            }
        }
        Ok(out)
    }

    fn cached_dir_listing_stale(
        pack_dir: &Path,
        dir_mt: SystemTime,
        cached: &CachedDir,
    ) -> Result<bool> {
        if cached.dir_mtime != dir_mt {
            return Ok(true);
        }
        // Pack directory mtimes often have one-second resolution; external `git gc` / repack can
        // replace `.idx` files without bumping the directory mtime (same wall-clock second).
        let on_disk = idx_paths_on_disk(pack_dir)?;
        let cached_paths: HashSet<PathBuf> = cached
            .indexes
            .iter()
            .map(|idx| idx.idx_path.clone())
            .collect();
        Ok(on_disk != cached_paths)
    }

    /// Re-scan `objects_dir`/`pack/` when its mtime changed since the cached listing (Git
    /// `reprepare_packed_git`), or when the set of pack index files no longer matches the
    /// cached listing. Returns true if the listing was refreshed.
    pub fn reprepare_dir_on_miss(objects_dir: &Path) -> Result<bool> {
        let pack_dir = objects_dir.join("pack");
        let dir_mt = dir_mtime(&pack_dir);
        let needs_rescan =
            crate::pack_store::PackStore::active_for_reads().with_catalog_read(|g| {
                match g.by_dir.get(&pack_dir) {
                    None => true,
                    Some(c) => cached_dir_listing_stale(&pack_dir, dir_mt, c).unwrap_or(true),
                }
            });
        if !needs_rescan {
            return Ok(false);
        }
        rescan_dir_indexes(objects_dir)?;
        Ok(true)
    }

    fn load_pack_bytes_from_disk(
        pack_path: &Path,
        mtime: SystemTime,
        size: u64,
    ) -> Result<Arc<PackData>> {
        invalidate_thread_local_for_pack_path_change(pack_path);
        let bytes = PackData::open(pack_path)?;
        let store = crate::pack_store::PackStore::active_for_reads();
        store.with_catalog(|g| {
            drop_delta_entries_locked(g, pack_path, store.delta_bases());
            g.by_idx.remove(&pack_path.with_extension("idx"));
            g.validated_pack_counts.remove(pack_path);
            g.by_pack.insert(
                pack_path.to_path_buf(),
                CachedPack {
                    mtime,
                    size,
                    bytes: Arc::clone(&bytes),
                },
            );
        });
        remember_pack_bytes(pack_path, &bytes);
        Ok(bytes)
    }

    /// Get the raw bytes of a pack file from cache, reading from disk only on cache miss.
    ///
    /// Cache hits skip `stat` for all pack basenames; in-process repack/gc clears the cache
    /// when packs change.
    pub fn get_pack_bytes(pack_path: &Path) -> Result<Arc<PackData>> {
        if let Some(hit) = cached_pack_bytes(pack_path) {
            return Ok(hit);
        }
        let store = crate::pack_store::PackStore::active_for_reads();
        if let Some(hit) =
            store.with_catalog_read(|g| g.by_pack.get(pack_path).map(|c| Arc::clone(&c.bytes)))
        {
            remember_pack_bytes(pack_path, &hit);
            return Ok(hit);
        }
        let (mtime, size) = file_signature(pack_path).ok_or_else(|| {
            Error::Io(io::Error::new(
                io::ErrorKind::NotFound,
                format!("pack not found: {}", pack_path.display()),
            ))
        })?;
        load_pack_bytes_from_disk(pack_path, mtime, size)
    }

    fn revalidate_stale_pack_bytes_in_context(pack_path: &Path) -> Result<bool> {
        let Some((mtime, size)) = file_signature(pack_path) else {
            let store = crate::pack_store::PackStore::active_for_reads();
            let removed = store.with_catalog(|g| {
                let removed = g.by_pack.remove(pack_path).is_some();
                if removed {
                    drop_delta_entries_locked(g, pack_path, store.delta_bases());
                }
                removed
            });
            invalidate_thread_local_for_pack_path_change(pack_path);
            super::drop_pack_read_info_cache_for(pack_path);
            return Ok(removed);
        };
        let store = crate::pack_store::PackStore::active_for_reads();
        let stale = store.with_catalog_read(|g| match g.by_pack.get(pack_path) {
            Some(c) => c.mtime != mtime || c.size != size,
            None => true,
        });
        if !stale {
            return Ok(false);
        }
        load_pack_bytes_from_disk(pack_path, mtime, size)?;
        super::drop_pack_read_info_cache_for(pack_path);
        Ok(true)
    }

    /// Re-read `pack_path` when its on-disk signature differs from the cached entry.
    ///
    /// Returns true when bytes were reloaded or the cache entry was dropped.
    pub fn revalidate_stale_pack_bytes(pack_path: &Path) -> Result<bool> {
        if crate::pack_store::PackStore::has_context() {
            return revalidate_stale_pack_bytes_in_context(pack_path);
        }
        let store = crate::pack_store::PackStore::legacy_for_pack_path(pack_path);
        crate::pack_store::PackStore::with_context(store, || {
            revalidate_stale_pack_bytes_in_context(pack_path)
        })
    }

    /// After a pack parse/decompress failure, reload from disk when cached bytes differ.
    ///
    /// Signature-only checks miss in-memory stale entries (tests) and same-second replacements
    /// with unchanged size; comparing the pack header and trailer fingerprint catches those
    /// without reading the whole file.
    ///
    /// Returns true when the cache was refreshed with disk contents.
    pub fn reload_pack_bytes_after_parse_failure(pack_path: &Path) -> Result<bool> {
        if revalidate_stale_pack_bytes(pack_path)? {
            return Ok(true);
        }
        let Some((mtime, size)) = file_signature(pack_path) else {
            return Ok(false);
        };
        let hash_bytes = hash_bytes_for_pack_file(pack_path)?;
        let disk_fp = fingerprint_from_file(pack_path, hash_bytes)?;
        let needs_reload = with_current(|g| {
            g.by_pack.get(pack_path).is_some_and(|c| {
                pack_fingerprint_mismatch(&c.bytes, hash_bytes, &disk_fp).unwrap_or(true)
            })
        });
        if !needs_reload {
            return Ok(false);
        }
        load_pack_bytes_from_disk(pack_path, mtime, size)?;
        super::drop_pack_read_info_cache_for(pack_path);
        Ok(true)
    }

    fn hash_bytes_for_pack_file(pack_path: &Path) -> Result<usize> {
        let idx_path = pack_path.with_extension("idx");
        if let Some(hash_bytes) =
            with_current(|g| g.by_idx.get(&idx_path).map(|c| c.idx.hash_bytes()))
        {
            return Ok(hash_bytes);
        }
        Ok(read_pack_index_no_verify(&idx_path)?.hash_bytes())
    }

    fn pack_fingerprint_mismatch(
        cached: &PackData,
        hash_bytes: usize,
        disk: &PackFingerprint,
    ) -> Result<bool> {
        Ok(cached.fingerprint(hash_bytes)? != *disk)
    }

    #[cfg(test)]
    pub fn test_reset_dir_rescan_count(pack_dir: &Path) {
        let objects_dir = pack_dir.parent().expect("pack dir under objects/");
        crate::pack_store::PackStore::legacy_for_objects_dir(objects_dir).with_pack(|g| {
            g.by_dir.remove(pack_dir);
            g.test_dir_rescan_counts.insert(pack_dir.to_path_buf(), 0);
        });
    }

    #[cfg(test)]
    pub fn test_reset_marker_stat_count() {
        TEST_MARKER_STAT_COUNT.with(|count| count.set(0));
    }

    #[cfg(test)]
    #[must_use]
    pub fn test_marker_stat_count() -> u64 {
        TEST_MARKER_STAT_COUNT.with(std::cell::Cell::get)
    }

    #[cfg(test)]
    #[must_use]
    pub fn test_dir_rescan_count(pack_dir: &Path) -> u64 {
        let objects_dir = pack_dir.parent().expect("pack dir under objects/");
        test_dir_rescan_count_on_store(
            &crate::pack_store::PackStore::legacy_for_objects_dir(objects_dir),
            pack_dir,
        )
    }

    #[cfg(test)]
    #[must_use]
    pub fn test_dir_rescan_count_on_store(
        store: &std::sync::Arc<crate::pack_store::PackStore>,
        pack_dir: &Path,
    ) -> u64 {
        store.with_pack(|g| g.test_dir_rescan_counts.get(pack_dir).copied().unwrap_or(0))
    }

    #[cfg(test)]
    pub fn test_reset_dir_rescan_count_on_store(
        store: &std::sync::Arc<crate::pack_store::PackStore>,
        pack_dir: &Path,
    ) {
        store.with_pack(|g| {
            g.by_dir.remove(pack_dir);
            g.test_dir_rescan_counts.insert(pack_dir.to_path_buf(), 0);
        });
    }

    /// Replace cached pack bytes (tests simulating a stale in-memory mapping).
    #[cfg(test)]
    pub fn test_inject_stale_pack_bytes(pack_path: &Path, stale: Arc<PackData>) {
        crate::pack_store::PackStore::legacy_for_pack_path(pack_path).with_pack(|g| {
            let stamp = g
                .by_pack
                .get(pack_path)
                .map(|c| (c.mtime, c.size))
                .unwrap_or((SystemTime::UNIX_EPOCH, stale.len() as u64));
            g.by_pack.insert(
                pack_path.to_path_buf(),
                CachedPack {
                    mtime: stamp.0,
                    size: stamp.1,
                    bytes: stale,
                },
            );
        });
    }

    fn clear_inner(g: &mut CatalogState) {
        g.by_dir.clear();
        g.by_idx.clear();
        g.by_pack.clear();
        g.pack_ids.clear();
    }

    /// Delta-base byte cap for the active read store (default [`DELTA_BASE_CACHE_DEFAULT`]).
    pub fn set_delta_base_cache_byte_limit(limit: usize) {
        crate::pack_store::PackStore::active_for_reads()
            .delta_bases()
            .set_byte_limit(limit);
    }

    #[cfg(test)]
    pub fn test_delta_base_cache_bytes_used(pack_path: &Path) -> usize {
        crate::pack_store::PackStore::legacy_for_pack_path(pack_path)
            .delta_bases()
            .bytes_used()
    }

    #[cfg(test)]
    pub fn test_set_delta_base_cache_byte_limit(pack_path: &Path, limit: usize) {
        crate::pack_store::PackStore::legacy_for_pack_path(pack_path)
            .delta_bases()
            .set_byte_limit(limit);
    }

    #[cfg(test)]
    pub fn test_delta_base_cached(pack_path: &Path, pack_id: u32, offset: u64) -> bool {
        crate::pack_store::PackStore::legacy_for_pack_path(pack_path)
            .delta_bases()
            .contains(pack_id, offset)
    }

    fn pack_id_for_in_context(pack_path: &Path) -> u32 {
        let store = crate::pack_store::PackStore::active_for_reads();
        if let Some(id) = PACK_IDS.with(|cell| cell.borrow().get(pack_path).copied()) {
            return id;
        }
        if let Some(id) = store.with_catalog_read(|g| g.pack_ids.get(pack_path).copied()) {
            PACK_IDS.with(|cell| {
                cell.borrow_mut().insert(pack_path.to_path_buf(), id);
            });
            return id;
        }
        store.with_catalog(|g| {
            if let Some(&id) = g.pack_ids.get(pack_path) {
                return id;
            }
            let id = g.next_pack_id;
            g.next_pack_id = g.next_pack_id.saturating_add(1);
            g.pack_ids.insert(pack_path.to_path_buf(), id);
            PACK_IDS.with(|cell| {
                cell.borrow_mut().insert(pack_path.to_path_buf(), id);
            });
            id
        })
    }

    /// Stable small id for `pack_path`, assigned on first use.
    pub fn pack_id_for(pack_path: &Path) -> u32 {
        if crate::pack_store::PackStore::has_context() {
            return pack_id_for_in_context(pack_path);
        }
        let store = crate::pack_store::PackStore::legacy_for_pack_path(pack_path);
        crate::pack_store::PackStore::with_context(store, || pack_id_for_in_context(pack_path))
    }

    /// Drop every cached delta base inflated from `pack_path`.
    fn drop_delta_entries_locked(
        g: &mut CatalogState,
        pack_path: &Path,
        delta: &ShardedDeltaBaseLru,
    ) {
        if let Some(&pack_id) = g.pack_ids.get(pack_path) {
            delta.drop_pack(pack_id);
        }
    }

    /// Cached resolved object at `(pack_id, offset)`, if still resident.
    pub fn get_delta_base(pack_id: u32, offset: u64) -> Option<(ObjectKind, Arc<[u8]>)> {
        crate::pack_store::PackStore::active_for_reads()
            .delta_bases()
            .get(DeltaCacheKey { pack_id, offset })
    }

    #[must_use]
    pub fn delta_base_caching_enabled() -> bool {
        crate::pack_store::PackStore::active_for_reads()
            .delta_bases()
            .caching_enabled()
    }

    /// Insert many intermediate bases (hot path for delta-chain resolution).
    pub fn put_delta_bases(kind: ObjectKind, items: &[(u32, u64, Arc<[u8]>)]) {
        if items.is_empty() {
            return;
        }
        crate::pack_store::PackStore::active_for_reads()
            .delta_bases()
            .put_many(kind, items);
    }

    /// Drop only cached delta bases (pack indexes and bytes stay cached).
    pub fn clear_delta_bases() {
        crate::pack_store::PackStore::active_for_reads()
            .delta_bases()
            .clear();
    }

    /// Re-stamp the cached signature for `pack_path` after the caller deliberately touched the
    /// file's mtime (object freshening). Pack contents are immutable for a given pack name, so
    /// a self-inflicted mtime bump must not evict the cached bytes — without this, every
    /// `odb.write` of an already-packed object forced a full re-read of the pack on the next
    /// lookup. We bump the cached mtime to now without `stat` (the caller just utime'd the file).
    /// External modifications still invalidate via [`revalidate_stale_pack_bytes`].
    pub fn refresh_pack_signature(pack_path: &Path, touched_at: SystemTime) {
        with_current(|g| {
            if let Some(c) = g.by_pack.get_mut(pack_path) {
                c.mtime = touched_at;
            }
        });
    }
}

pub use pack_cache::revalidate_stale_pack_bytes;

/// Read all pack indexes under `<objects_dir>/pack/` from the process-wide cache.
///
/// Cached reads skip the `.idx` SHA-1 trailer verification that [`read_pack_index`]
/// performs; corruption checks happen during `fsck`/`verify-pack`, not on every object
/// lookup (matches Git). The directory listing is prepared once per process and
/// re-scanned only on a lookup miss when the pack directory's mtime changed (Git
/// `reprepare_packed_git`), or when [`clear_pack_cache`] drops the cache after repack/gc.
///
/// # Errors
///
/// Returns [`Error::Io`] when the directory cannot be enumerated.
pub fn read_local_pack_indexes_cached(objects_dir: &Path) -> Result<Vec<Arc<PackIndex>>> {
    if crate::pack_store::PackStore::has_context() {
        return pack_cache::get_dir_indexes(objects_dir);
    }
    let store = crate::pack_store::PackStore::legacy_for_objects_dir(objects_dir);
    crate::pack_store::PackStore::with_context(store, || pack_cache::get_dir_indexes(objects_dir))
}

/// Re-scan the pack directory when its mtime changed since the cached listing.
///
/// Called when an object was not found in loose storage or any known pack; returns
/// `true` when the listing was refreshed so the caller can retry once.
///
/// # Errors
///
/// Returns [`Error::Io`] when the pack directory cannot be read.
pub fn reprepare_pack_directory_on_miss(objects_dir: &Path) -> Result<bool> {
    if crate::pack_store::PackStore::has_context() {
        return reprepare_pack_directory_on_miss_in_context(objects_dir);
    }
    let store = crate::pack_store::PackStore::legacy_for_objects_dir(objects_dir);
    crate::pack_store::PackStore::with_context(store, || {
        reprepare_pack_directory_on_miss_in_context(objects_dir)
    })
}

fn reprepare_pack_directory_on_miss_in_context(objects_dir: &Path) -> Result<bool> {
    let refreshed = pack_cache::reprepare_dir_on_miss(objects_dir)?;
    if refreshed {
        crate::midx::evict_midx_read_cache_for_pack_dir(&objects_dir.join("pack"));
    }
    Ok(refreshed)
}

/// Read a single pack index from the process-wide cache (parses from disk on miss).
/// Skips trailer verification.
///
/// # Errors
///
/// Returns [`Error::Io`] when the file is missing or [`Error::CorruptObject`] for
/// malformed indexes.
pub fn read_pack_index_cached(idx_path: &Path) -> Result<Arc<PackIndex>> {
    if crate::pack_store::PackStore::has_context() {
        return pack_cache::get_index(idx_path);
    }
    let pack_path = idx_path.with_extension("pack");
    let store = crate::pack_store::PackStore::legacy_for_pack_path(&pack_path);
    crate::pack_store::PackStore::with_context(store, || pack_cache::get_index(idx_path))
}

/// Read pack file bytes from the process-wide cache.
///
/// # Errors
///
/// Returns [`Error::Io`] when the pack cannot be read.
pub fn read_pack_bytes_cached(pack_path: &Path) -> Result<Arc<PackData>> {
    if crate::pack_store::PackStore::has_context() {
        return pack_cache::get_pack_bytes(pack_path);
    }
    let store = crate::pack_store::PackStore::legacy_for_pack_path(pack_path);
    crate::pack_store::PackStore::with_context(store, || pack_cache::get_pack_bytes(pack_path))
}

/// Drop pack/MIDX caches on every registered [`PackStore`](crate::pack_store::PackStore).
///
/// Prefer [`Odb::invalidate_packs`](crate::odb::Odb::invalidate_packs) when an [`Odb`](crate::odb::Odb)
/// handle is available. Tests and benchmarks use this to reset all stores in the process.
pub fn clear_pack_cache() {
    #[cfg(test)]
    let _guard = pack_cache_test_guard();
    clear_pack_read_info_cache();
    pack_cache::clear_thread_local_dir_indexes();
    crate::pack_store::PackStore::invalidate_all_legacy();
}

/// Test helper: reset pack marker stat counter (integration tests only).
#[cfg(test)]
pub fn test_reset_pack_marker_stat_count() {
    pack_cache::test_reset_marker_stat_count();
}

/// Test helper: read pack marker stat counter (integration tests only).
#[cfg(test)]
#[must_use]
pub fn test_pack_marker_stat_count() -> u64 {
    pack_cache::test_marker_stat_count()
}

/// Drop only the process-wide delta-base LRU (indexes and pack bytes stay cached).
pub fn clear_pack_delta_base_cache() {
    pack_cache::clear_delta_bases();
}

/// Apply `core.deltaBaseCacheLimit` from repository config to the process-wide delta-base LRU.
pub fn configure_delta_base_cache_from_config(cfg: Option<&crate::config::ConfigSet>) {
    let bytes = cfg
        .and_then(|c| c.get("core.deltaBaseCacheLimit"))
        .as_ref()
        .and_then(|v| crate::config::parse_i64(v).ok())
        .and_then(|n| usize::try_from(n).ok())
        .unwrap_or(pack_cache::DELTA_BASE_CACHE_DEFAULT);
    pack_cache::set_delta_base_cache_byte_limit(bytes);
}

/// Re-stamp the cached pack-bytes signature after deliberately touching `pack_path`'s mtime
/// (object freshening). See the internal `pack_cache::refresh_pack_signature` helper.
pub fn refresh_pack_bytes_signature(pack_path: &Path, touched_at: SystemTime) {
    pack_cache::refresh_pack_signature(pack_path, touched_at);
}

/// Collect aggregate local pack metrics.
///
/// # Errors
///
/// Returns [`Error::Io`] when reading pack metadata fails.
pub fn collect_local_pack_info(objects_dir: &Path) -> Result<LocalPackInfo> {
    let indexes = read_local_pack_indexes(objects_dir)?;
    let mut info = LocalPackInfo::default();
    for idx in indexes {
        let pack_meta = fs::metadata(&idx.pack_path).map_err(Error::Io)?;
        let idx_meta = fs::metadata(&idx.idx_path).map_err(Error::Io)?;
        info.pack_count += 1;
        info.object_count += idx.len();
        info.size_bytes += pack_meta.len() + idx_meta.len();
        let hash_bytes = idx.hash_bytes();
        for entry in idx.iter() {
            if entry.oid().len() == hash_bytes {
                if let Ok(oid) = ObjectId::from_bytes(entry.oid()) {
                    info.object_ids.insert(oid);
                }
            }
        }
    }
    Ok(info)
}

/// Write a version-2 `.idx` for `entries` describing objects in `pack_path`.
///
/// `entries` lists `(oid, byte_offset)` pairs; they are sorted by OID before
/// writing. The pack file must already exist on disk with a valid trailing
/// checksum at the repository hash width `hash_bytes` (`20` or `32`).
///
/// # Errors
///
/// Returns [`Error::CorruptObject`] when the pack is too small or an entry
/// offset cannot be walked, or [`Error::Io`] on filesystem failure.
/// Write a v2 pack index file.
///
/// `sort_threads` controls OID-table sorting: `None` or `Some(1)` uses a single thread;
/// `Some(n)` with `n > 1` may sort chunks on up to `n` workers (for large entry counts).
pub fn write_v2_pack_index_with_trailer(
    idx_path: &Path,
    entries: &[(ObjectId, u64, u32)],
    pack_trailer: &[u8],
    hash_bytes: usize,
    sort_threads: Option<usize>,
) -> Result<()> {
    write_v2_pack_index_sorting(
        idx_path,
        &mut entries.to_vec(),
        pack_trailer,
        hash_bytes,
        sort_threads,
    )
}

/// `write_v2_pack_index` function.
pub fn write_v2_pack_index(
    idx_path: &Path,
    pack_path: &Path,
    entries: &[(ObjectId, u64, u32)],
    hash_bytes: usize,
) -> Result<()> {
    let pack_bytes = fs::read(pack_path).map_err(Error::Io)?;
    if pack_bytes.len() < hash_bytes {
        return Err(Error::CorruptObject(
            "pack too small for idx trailer".into(),
        ));
    }
    let trailer = &pack_bytes[pack_bytes.len() - hash_bytes..];
    write_v2_pack_index_with_trailer(idx_path, entries, trailer, hash_bytes, None)
}

const PARALLEL_INDEX_SORT_MIN: usize = 8_192;

/// Sort `entries` by object id in place and return the v2 fanout table.
///
/// Entries are first partitioned in place by their leading byte (which also yields the
/// fanout), then contiguous runs of buckets are sorted on up to `sort_threads` workers.
/// No copy of the entries is made.
fn sort_index_entries(
    entries: &mut [(ObjectId, u64, u32)],
    sort_threads: Option<usize>,
) -> [u32; 256] {
    let lead = |entry: &(ObjectId, u64, u32)| usize::from(entry.0.as_bytes()[0]);
    let mut counts = [0usize; 256];
    for entry in entries.iter() {
        counts[lead(entry)] += 1;
    }
    let mut ends = [0usize; 256];
    let mut total = 0usize;
    for (end, count) in ends.iter_mut().zip(counts) {
        total += count;
        *end = total;
    }
    let mut next = [0usize; 256];
    next[1..].copy_from_slice(&ends[..255]);
    for bucket in 0..256 {
        while next[bucket] < ends[bucket] {
            let at = next[bucket];
            let home = lead(&entries[at]);
            if home == bucket {
                next[bucket] += 1;
            } else {
                entries.swap(at, next[home]);
                next[home] += 1;
            }
        }
    }

    let by_oid =
        |a: &(ObjectId, u64, u32), b: &(ObjectId, u64, u32)| a.0.as_bytes().cmp(b.0.as_bytes());
    let threads = sort_threads.unwrap_or(1).max(1);
    if threads <= 1 || entries.len() < PARALLEL_INDEX_SORT_MIN {
        let mut start = 0;
        for end in ends {
            entries[start..end].sort_unstable_by(by_oid);
            start = end;
        }
    } else {
        let share = entries.len().div_ceil(threads);
        std::thread::scope(|scope| {
            let mut rest = &mut *entries;
            let mut taken = 0usize;
            for end in ends {
                if end - taken >= share || end == total {
                    let (run, tail) = std::mem::take(&mut rest).split_at_mut(end - taken);
                    rest = tail;
                    taken = end;
                    if run.len() > 1 {
                        scope.spawn(move || run.sort_unstable_by(by_oid));
                    }
                }
            }
        });
    }
    ends.map(|end| u32::try_from(end).unwrap_or(u32::MAX))
}

/// Sort `entries` in place and write them as a version-2 pack index at `idx_path`.
///
/// Streams the file through its checksum instead of building it in memory, so the only
/// large allocation is the caller's `entries`.
///
/// # Errors
///
/// Returns [`Error::CorruptObject`] when an object id or the trailer does not have
/// `hash_bytes` bytes, or the large-offset table overflows, and [`Error::Io`] on write
/// failure.
pub(crate) fn write_v2_pack_index_sorting(
    idx_path: &Path,
    entries: &mut [(ObjectId, u64, u32)],
    pack_trailer: &[u8],
    hash_bytes: usize,
    sort_threads: Option<usize>,
) -> Result<()> {
    use std::io::Write;

    let Some(algo) = HashAlgo::from_len(hash_bytes) else {
        return Err(Error::CorruptObject(format!(
            "unsupported index hash width {hash_bytes}"
        )));
    };
    if pack_trailer.len() != hash_bytes {
        return Err(Error::CorruptObject(format!(
            "pack trailer length {} does not match hash width {hash_bytes}",
            pack_trailer.len()
        )));
    }
    if let Some((oid, _, _)) = entries
        .iter()
        .find(|(oid, _, _)| oid.as_bytes().len() != hash_bytes)
    {
        return Err(Error::CorruptObject(format!(
            "OID width {} does not match index hash width {hash_bytes}",
            oid.as_bytes().len()
        )));
    }
    let fanout = sort_index_entries(entries, sort_threads);

    let file = fs::File::create(idx_path).map_err(Error::Io)?;
    let mut out = std::io::BufWriter::with_capacity(256 * 1024, file);
    let mut hasher = algo.checksum_hasher();
    let mut put = |bytes: &[u8]| -> Result<()> {
        hasher.update(bytes);
        out.write_all(bytes).map_err(Error::Io)
    };
    put(b"\xfftOc")?;
    put(&2u32.to_be_bytes())?;
    for f in fanout {
        put(&f.to_be_bytes())?;
    }
    for (oid, _, _) in entries.iter() {
        put(oid.as_bytes())?;
    }
    for (_, _, crc) in entries.iter() {
        put(&crc.to_be_bytes())?;
    }
    let mut large = 0u32;
    for (_, off, _) in entries.iter() {
        match u32::try_from(*off) {
            Ok(small) if small < 0x8000_0000 => put(&small.to_be_bytes())?,
            _ => {
                if large >= 0x8000_0000 {
                    return Err(Error::CorruptObject(
                        "pack index large-offset table overflow".to_owned(),
                    ));
                }
                put(&(0x8000_0000u32 | large).to_be_bytes())?;
                large += 1;
            }
        }
    }
    for (_, off, _) in entries.iter() {
        if *off >= 0x8000_0000 {
            put(&off.to_be_bytes())?;
        }
    }
    put(pack_trailer)?;
    let digest = hasher.finalize();
    out.write_all(&digest).map_err(Error::Io)?;
    out.flush().map_err(Error::Io)?;
    Ok(())
}

#[must_use]
/// `oid_bytes_to_hex` function.
pub fn oid_bytes_to_hex(oid: &[u8]) -> String {
    hex::encode(oid)
}

/// Hash canonical loose object bytes (`kind SP size NUL data`) with the repo hash width.
pub fn hash_object_bytes(kind: ObjectKind, data: &[u8], hash_bytes: usize) -> Result<Vec<u8>> {
    let Some(algo) = HashAlgo::from_len(hash_bytes) else {
        return Err(Error::CorruptObject(format!(
            "unsupported object hash width: {hash_bytes}"
        )));
    };
    Ok(crate::hash::hash_object(algo, kind, data)?
        .as_bytes()
        .to_vec())
}

/// Parse a pack index file (version 1 legacy or version 2), verifying the SHA-1
/// trailer checksum.
///
/// Used by `fsck`/`verify-pack` and similar code that wants on-disk validation. Hot
/// object-lookup paths should call [`read_pack_index_cached`] (which skips trailer
/// verification, matching Git's normal read path).
///
/// # Errors
///
/// Returns [`Error::CorruptObject`] when format checks fail.
pub fn read_pack_index(idx_path: &Path) -> Result<PackIndex> {
    crate::pack_index::open_pack_index_file(idx_path, true)
}

/// Parse a pack index file without verifying the SHA-1 trailer checksum.
///
/// Git reads the `.idx` offset table without re-checking its trailer in the MIDX
/// write path (`midx-write.c`/`packfile.c` `open_pack_index`), so a deliberately
/// corrupted-but-structurally-valid idx (t5319 64-bit offset tests) still loads.
pub fn read_pack_index_no_verify(idx_path: &Path) -> Result<PackIndex> {
    crate::pack_index::open_pack_index_file(idx_path, false)
}

/// A pack object type as encoded in the packed stream header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackedType {
    /// Commit object.
    Commit,
    /// Tree object.
    Tree,
    /// Blob object.
    Blob,
    /// Tag object.
    Tag,
    /// Offset delta.
    OfsDelta,
    /// Reference delta.
    RefDelta,
}

impl PackedType {
    /// Printable name used by `verify-pack -v` output.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Commit => "commit",
            Self::Tree => "tree",
            Self::Blob => "blob",
            Self::Tag => "tag",
            Self::OfsDelta => "ofs-delta",
            Self::RefDelta => "ref-delta",
        }
    }
}

/// A decoded object header record used by `verify-pack`.
#[derive(Debug, Clone)]
pub struct VerifyObjectRecord {
    /// Object ID from the index (20 or 32 raw bytes).
    pub oid: Vec<u8>,
    /// Type from the pack stream header.
    pub packed_type: PackedType,
    /// Uncompressed object size from the pack header.
    pub size: u64,
    /// Total bytes in pack occupied by this object slot.
    pub size_in_pack: u64,
    /// Offset in pack file.
    pub offset: u64,
    /// Delta chain depth, if deltified.
    pub depth: Option<u64>,
    /// Base object for ref-delta objects.
    pub base_oid: Option<Vec<u8>>,
    /// In-pack base offset for ofs-delta objects.
    pub base_pack_offset: Option<u64>,
}

/// How a delta object in a pack references its base, used to compute chain depth order-independently.
enum DeltaBaseLink {
    /// `REF_DELTA`: base identified by raw object id (20 or 32 bytes).
    Oid(Vec<u8>),
    /// `OFS_DELTA`: base identified by its absolute offset in the pack.
    Offset(u64),
}

/// Resolve the delta-chain depth of record `i`, memoizing the result into `records[i].depth`.
///
/// Full (non-delta) objects have depth 0. A delta's depth is one greater than its base's depth.
/// Following base links by offset/oid makes this independent of the order objects appear in the
/// pack — a ref-delta's base may be stored *after* the delta itself. A base that is not present in
/// this pack (thin pack) or a cycle is treated as depth 0 for the missing/looping link.
///
/// # Errors
///
/// Returns [`Error::CorruptObject`] when a ref-delta record is missing its base oid.
fn resolve_delta_depth(
    i: usize,
    base_links: &[Option<DeltaBaseLink>],
    by_oid: &HashMap<Vec<u8>, usize>,
    by_offset_idx: &HashMap<u64, usize>,
    records: &mut [VerifyObjectRecord],
) -> Result<u64> {
    if let Some(d) = records[i].depth {
        return Ok(d);
    }
    let Some(link) = &base_links[i] else {
        return Ok(0);
    };
    let base_idx = match link {
        DeltaBaseLink::Oid(oid) => by_oid.get(oid).copied(),
        DeltaBaseLink::Offset(off) => by_offset_idx.get(off).copied(),
    };
    // Mark this record visited before recursing so a malformed cyclic chain cannot recurse forever.
    records[i].depth = Some(1);
    let depth = match base_idx {
        Some(b) if b != i => {
            resolve_delta_depth(b, base_links, by_oid, by_offset_idx, records)?.saturating_add(1)
        }
        // Base absent from this pack (thin) or self-referential: count this delta as depth 1.
        _ => 1,
    };
    records[i].depth = Some(depth);
    Ok(depth)
}

/// Default in-pack delta chain depth when `pack.depth` is unset (matches Git).
pub const DEFAULT_PACK_DEPTH: usize = 50;

/// Longest delta-chain depth among verify-pack records (number of edges from base to tip).
#[must_use]
pub fn max_verify_pack_delta_depth(records: &[VerifyObjectRecord]) -> u64 {
    records.iter().filter_map(|r| r.depth).max().unwrap_or(0)
}

/// Break only delta chains longer than `max_depth` edges in a target→base map.
///
/// Each entry maps a delta object to its chosen in-pack base. Chains already within
/// `max_depth` keep their original bases unchanged. Longer chains are broken on Git's
/// `(depth + 1)` modulo rule by removing only the delta edges at cut points; links
/// inside each resulting segment stay as originally chosen (matching `break_delta_chains`).
pub fn apply_delta_depth_limit(map: &mut HashMap<ObjectId, ObjectId>, max_depth: usize) {
    let keys: Vec<ObjectId> = map.keys().copied().collect();
    let value_set: HashSet<ObjectId> = map.values().copied().collect();
    let mut tips: Vec<ObjectId> = keys
        .into_iter()
        .filter(|k| !value_set.contains(k))
        .collect();
    tips.sort();

    let modulus = max_depth.saturating_add(1);
    let mut snip: HashSet<ObjectId> = HashSet::new();

    for tip in tips {
        let mut chain: Vec<ObjectId> = Vec::new();
        let mut cur = tip;
        let mut seen = HashSet::new();
        while seen.insert(cur) {
            chain.push(cur);
            let Some(&b) = map.get(&cur) else {
                break;
            };
            cur = b;
        }

        let n = chain.len();
        if n < 2 || n - 1 <= max_depth {
            continue;
        }

        let mut total_depth = (n - 1) as u32;
        for &oid in &chain {
            let assigned = (total_depth as usize) % modulus;
            total_depth = total_depth.saturating_sub(1);
            if assigned == 0 {
                snip.insert(oid);
            }
        }
    }

    for oid in snip {
        map.remove(&oid);
    }
}

/// Pack/index pair validated for fsck without inflating every object.
///
/// Structure, checksums, and per-entry headers are checked; object hashes are
/// verified separately when objects are read for semantic fsck.
#[derive(Debug)]
pub(crate) struct VerifiedPackForFsck {
    pub idx: PackIndex,
    /// Memory-mapped pack file (owned only for tiny packs or when mapping fails).
    pub pack_bytes: Arc<PackData>,
    pub records: Vec<VerifyObjectRecord>,
}

/// Verify pack trailer, index checksum, counts, and object header layout.
///
/// Does not inflate objects or verify per-object hashes (see [`verify_pack_and_collect`]).
///
/// # Errors
///
/// Returns [`Error::CorruptObject`] when the index or pack are malformed.
pub(crate) fn verify_pack_for_fsck(idx_path: &Path) -> Result<VerifiedPackForFsck> {
    let (idx, pack_bytes, records) = verify_pack_structure_and_records(idx_path, true)?;
    Ok(VerifiedPackForFsck {
        idx,
        pack_bytes,
        records,
    })
}

/// Pack-level trailer/index checksum problem detected while preparing object fsck.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PackFsckTrailerIssue {
    /// Pack file trailing hash does not match payload.
    PackChecksum,
    /// Embedded pack hash in the `.idx` does not match the pack file.
    IdxMismatch,
}

/// Load pack bytes and header records for per-object fsck even when the pack trailer is wrong.
///
/// Returns a trailer/index checksum issue separately so callers can still inflate objects.
pub(crate) fn prepare_pack_for_object_fsck(
    idx_path: &Path,
) -> Result<(VerifiedPackForFsck, Option<PackFsckTrailerIssue>)> {
    let (idx, pack_bytes, records) = verify_pack_structure_and_records(idx_path, false)?;
    let trailer = pack_trailer_fsck_issue(idx_path, &idx, &pack_bytes);
    Ok((
        VerifiedPackForFsck {
            idx,
            pack_bytes,
            records,
        },
        trailer,
    ))
}

fn pack_trailer_fsck_issue(
    idx_path: &Path,
    idx: &PackIndex,
    pack_bytes: &[u8],
) -> Option<PackFsckTrailerIssue> {
    let hb = idx.hash_bytes();
    let algo = HashAlgo::from_len(hb)?;
    if verify_trailer(algo, pack_bytes).is_err() {
        return Some(PackFsckTrailerIssue::PackChecksum);
    }
    let idx_file_bytes = fs::read(idx_path).ok()?;
    if idx_file_bytes.len() >= 2 * hb {
        let pack_end = pack_bytes.len().checked_sub(hb)?;
        let n = idx_file_bytes.len();
        let embedded = &idx_file_bytes[n - 2 * hb..n - hb];
        if embedded != &pack_bytes[pack_end..] {
            return Some(PackFsckTrailerIssue::IdxMismatch);
        }
    }
    None
}

/// Verify one pack/index pair and optionally return object records.
///
/// # Errors
///
/// Returns [`Error::CorruptObject`] when the index or pack are malformed.
pub fn verify_pack_and_collect(idx_path: &Path) -> Result<Vec<VerifyObjectRecord>> {
    let VerifiedPackForFsck {
        idx,
        pack_bytes,
        records,
    } = verify_pack_for_fsck(idx_path)?;
    let hb = idx.hash_bytes();
    for entry in idx.iter() {
        let obj = read_object_from_pack_bytes(&pack_bytes, &idx, entry.oid())?;
        let computed = hash_object_bytes(obj.kind, &obj.data, hb)?;
        if computed.as_slice() != entry.oid() {
            return Err(Error::CorruptObject(format!(
                "pack object hash mismatch at offset {} (index says {})",
                entry.offset()?,
                oid_bytes_to_hex(entry.oid())
            )));
        }
    }
    Ok(records)
}

fn verify_pack_structure_and_records(
    idx_path: &Path,
    verify_pack_trailer: bool,
) -> Result<(PackIndex, Arc<PackData>, Vec<VerifyObjectRecord>)> {
    let idx = read_pack_index(idx_path)?;
    let idx_file_bytes = fs::read(idx_path).map_err(Error::Io)?;
    let pack_bytes = PackData::open_with_min_len(&idx.pack_path, 0)?;
    let hb = idx.hash_bytes();
    if pack_bytes.len() < 12 + hb {
        return Err(Error::CorruptObject(format!(
            "pack file {} is too small",
            idx.pack_path.display()
        )));
    }
    let pack_end = pack_bytes.len() - hb;
    let Some(algo) = HashAlgo::from_len(hb) else {
        return Err(Error::CorruptObject(format!(
            "unsupported OID width {hb} for pack {}",
            idx.pack_path.display()
        )));
    };
    if verify_pack_trailer {
        verify_trailer(algo, &pack_bytes).map_err(|e| {
            Error::CorruptObject(format!(
                "pack trailing checksum mismatch for {}: {e}",
                idx.pack_path.display()
            ))
        })?;
        // The `.idx` ends with the pack checksum followed by its own checksum, both
        // at the repository hash width `hb` (20 for SHA-1, 32 for SHA-256).
        if idx_file_bytes.len() >= 2 * hb {
            let n = idx_file_bytes.len();
            let embedded = &idx_file_bytes[n - 2 * hb..n - hb];
            if embedded != &pack_bytes[pack_end..] {
                return Err(Error::CorruptObject(format!(
                    "pack checksum in index does not match {}",
                    idx.pack_path.display()
                )));
            }
        }
    }
    if &pack_bytes[0..4] != b"PACK" {
        return Err(Error::CorruptObject(format!(
            "pack file {} has invalid signature",
            idx.pack_path.display()
        )));
    }
    let version = u32::from_be_bytes(pack_bytes[4..8].try_into().unwrap_or([0, 0, 0, 0]));
    if version != 2 && version != 3 {
        return Err(Error::CorruptObject(format!(
            "unsupported pack version {} in {}",
            version,
            idx.pack_path.display()
        )));
    }
    let count = u32::from_be_bytes(pack_bytes[8..12].try_into().unwrap_or([0, 0, 0, 0])) as usize;
    if count != idx.len() {
        return Err(Error::CorruptObject(format!(
            "pack/index object count mismatch for {}",
            idx.pack_path.display()
        )));
    }

    let mut by_offset: BTreeMap<u64, Vec<u8>> = BTreeMap::new();
    for entry in idx.iter() {
        by_offset.insert(entry.offset()?, entry.oid().to_vec());
    }
    let offsets: Vec<u64> = by_offset.keys().copied().collect();
    if offsets.is_empty() {
        return Ok((idx, pack_bytes, Vec::new()));
    }

    let mut by_oid: HashMap<Vec<u8>, usize> = HashMap::new();
    let mut by_offset_idx: HashMap<u64, usize> = HashMap::new();
    let mut records: Vec<VerifyObjectRecord> = Vec::with_capacity(offsets.len());
    // Per-record base pointer for delta objects, captured while scanning headers and resolved into
    // chain depths afterwards. Delta bases can appear *after* the delta in pack order (ref-deltas in
    // particular), so depth must be computed by following these pointers, not in scan order.
    let mut base_links: Vec<Option<DeltaBaseLink>> = Vec::with_capacity(offsets.len());
    for (i, offset) in offsets.iter().copied().enumerate() {
        let oid = by_offset.get(&offset).cloned().ok_or_else(|| {
            Error::CorruptObject(format!("missing object id for offset {}", offset))
        })?;
        let next_off = offsets
            .get(i + 1)
            .copied()
            .unwrap_or((pack_bytes.len() - hb) as u64);
        if next_off <= offset || next_off > (pack_bytes.len() - hb) as u64 {
            return Err(Error::CorruptObject(format!(
                "invalid object boundaries at offset {} in {}",
                offset,
                idx.pack_path.display()
            )));
        }
        let mut p = offset as usize;
        let (packed_type, size) = parse_pack_object_header(&pack_bytes, &mut p)?;
        let mut base_oid: Option<Vec<u8>> = None;
        let mut base_link: Option<DeltaBaseLink> = None;

        match packed_type {
            PackedType::RefDelta => {
                if p + hb > pack_bytes.len() {
                    return Err(Error::CorruptObject(format!(
                        "truncated ref-delta base at offset {}",
                        offset
                    )));
                }
                let raw = pack_bytes[p..p + hb].to_vec();
                base_oid = Some(raw.clone());
                base_link = Some(DeltaBaseLink::Oid(raw));
            }
            PackedType::OfsDelta => {
                let base_offset = parse_ofs_delta_base(&pack_bytes, &mut p, offset)?;
                base_link = Some(DeltaBaseLink::Offset(base_offset));
            }
            PackedType::Commit | PackedType::Tree | PackedType::Blob | PackedType::Tag => {}
        }

        let size_in_pack = next_off - offset;
        let base_pack_offset = match &base_link {
            Some(DeltaBaseLink::Offset(off)) => Some(*off),
            _ => None,
        };
        records.push(VerifyObjectRecord {
            oid: oid.clone(),
            packed_type,
            size,
            size_in_pack,
            offset,
            depth: None,
            base_oid,
            base_pack_offset,
        });
        base_links.push(base_link);
        by_oid.insert(oid, i);
        by_offset_idx.insert(offset, i);
    }

    // Resolve delta chain depths by following base links to their record index, regardless of the
    // order objects appear in the pack. A delta's depth is one more than its base's depth; full
    // objects have depth 0 (represented as `None` in the record). Memoize to keep this O(n).
    for i in 0..records.len() {
        if base_links[i].is_some() {
            let _ = resolve_delta_depth(i, &base_links, &by_oid, &by_offset_idx, &mut records)?;
        }
    }

    Ok((idx, pack_bytes, records))
}

/// Read alternates recursively, deduplicated in discovery order.
///
/// # Errors
///
/// Returns [`Error::Io`] when alternate files cannot be read.
pub fn read_alternates_recursive(objects_dir: &Path) -> Result<Vec<PathBuf>> {
    let mut visited = HashSet::new();
    let mut out = Vec::new();
    read_alternates_inner(objects_dir, &mut visited, &mut out, 0)?;
    Ok(out)
}

/// Maximum alternate chain depth (git uses 5).
const MAX_ALTERNATE_DEPTH: usize = 5;

/// Parse one non-empty line from `objects/info/alternates`.
///
/// Blank lines and `#` comments are ignored. Paths may be absolute, relative to the
/// current `objects/` directory, or C-style quoted (Git `read_info/alternates`).
fn parse_alternates_file_line(raw: &str) -> Option<PathBuf> {
    let line = raw.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    if line.starts_with('"') {
        let path = unquote_alternates_c_style(line)?;
        if path.is_empty() {
            return None;
        }
        return Some(PathBuf::from(path));
    }
    Some(PathBuf::from(line))
}

/// Unquote a double-quoted alternates path (Git `unquote_c_style` subset).
fn unquote_alternates_c_style(line: &str) -> Option<String> {
    let bytes = line.as_bytes();
    if bytes.first() != Some(&b'"') {
        return None;
    }
    let mut out = Vec::new();
    let mut i = 1usize;
    while i < bytes.len() {
        if bytes[i] == b'"' {
            return String::from_utf8(out).ok();
        }
        if bytes[i] == b'\\' {
            i += 1;
            if i >= bytes.len() {
                return None;
            }
            match bytes[i] {
                b'\\' => out.push(b'\\'),
                b'"' => out.push(b'"'),
                b'a' => out.push(7),
                b'b' => out.push(8),
                b'f' => out.push(12),
                b'n' => out.push(b'\n'),
                b'r' => out.push(b'\r'),
                b't' => out.push(b'\t'),
                b'v' => out.push(11),
                c if c.is_ascii_digit() => {
                    if i + 2 >= bytes.len() {
                        return None;
                    }
                    let oct = std::str::from_utf8(&bytes[i..i + 3]).ok()?;
                    out.push(u8::from_str_radix(oct, 8).ok()?);
                    i += 2;
                }
                other => out.push(other),
            }
            i += 1;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    None
}

fn read_alternates_inner(
    objects_dir: &Path,
    visited: &mut HashSet<PathBuf>,
    out: &mut Vec<PathBuf>,
    depth: usize,
) -> Result<()> {
    if depth > MAX_ALTERNATE_DEPTH {
        return Ok(());
    }
    // Read the alternates file before canonicalizing: the canonical form is only needed to
    // resolve relative entries and deduplicate, and canonicalize walks every path component
    // (a readlink per ancestor) — pure waste in the overwhelmingly common no-alternates case.
    let alt_file = objects_dir.join("info").join("alternates");
    let text = match fs::read_to_string(&alt_file) {
        Ok(text) => text,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(Error::Io(err)),
    };
    let canonical = canonical_or_self(objects_dir);

    for raw in text.lines() {
        let Some(path) = parse_alternates_file_line(raw) else {
            continue;
        };
        let candidate = if path.is_absolute() {
            path
        } else {
            canonical.join(path)
        };
        let candidate = canonical_or_self(&candidate);
        if visited.insert(candidate.clone()) {
            out.push(candidate.clone());
            read_alternates_inner(&candidate, visited, out, depth + 1)?;
        }
    }
    Ok(())
}

fn canonical_or_self(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Convert a [`PackedType`] to an [`ObjectKind`] for non-delta types.
fn packed_type_to_kind(pt: PackedType) -> Result<ObjectKind> {
    match pt {
        PackedType::Commit => Ok(ObjectKind::Commit),
        PackedType::Tree => Ok(ObjectKind::Tree),
        PackedType::Blob => Ok(ObjectKind::Blob),
        PackedType::Tag => Ok(ObjectKind::Tag),
        PackedType::OfsDelta | PackedType::RefDelta => Err(Error::CorruptObject(
            "cannot convert delta type to object kind directly".to_owned(),
        )),
    }
}

// hygiene: per-thread pack zlib scratch and read-info cache (ROADMAP item 7 — pack read hot path)
thread_local! {
    static DELTA_SIZE_ZLIB: std::cell::RefCell<crate::zlib_inflate::ZlibInflateScratch> =
        std::cell::RefCell::new(crate::zlib_inflate::ZlibInflateScratch::default());
}
// hygiene: scoped batch read_info cache; cleared on pack invalidation and context exit
thread_local! {
    static PACK_READ_INFO_CACHE: std::cell::RefCell<HashMap<PackReadInfoCacheKey, ObjectInfo>> =
        std::cell::RefCell::new(HashMap::new());
}

const PACK_READ_INFO_CACHE_CAP: usize = 600_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct PackReadInfoCacheKey {
    pack_id: u32,
    offset: u64,
}

fn pack_read_info_cache_key(pack_path: &Path, offset: u64) -> Option<PackReadInfoCacheKey> {
    if !crate::pack_store::PackStore::has_context() {
        return None;
    }
    Some(PackReadInfoCacheKey {
        pack_id: pack_cache::pack_id_for(pack_path),
        offset,
    })
}

/// Drop cached batch `read_info` entries (pack context end, invalidation, repack).
pub(crate) fn clear_pack_read_info_cache() {
    PACK_READ_INFO_CACHE.with(|c| c.borrow_mut().clear());
}

fn drop_pack_read_info_cache_for(pack_path: &Path) {
    let pack_id = pack_cache::pack_id_for(pack_path);
    PACK_READ_INFO_CACHE.with(|c| {
        c.borrow_mut().retain(|k, _| k.pack_id != pack_id);
    });
}

fn cached_pack_read_info(pack_path: &Path, offset: u64) -> Option<ObjectInfo> {
    let key = pack_read_info_cache_key(pack_path, offset)?;
    PACK_READ_INFO_CACHE.with(|c| c.borrow().get(&key).cloned())
}

fn remember_pack_read_info(pack_path: &Path, offset: u64, info: ObjectInfo) {
    let Some(key) = pack_read_info_cache_key(pack_path, offset) else {
        return;
    };
    PACK_READ_INFO_CACHE.with(|c| {
        let mut g = c.borrow_mut();
        if g.len() >= PACK_READ_INFO_CACHE_CAP {
            g.clear();
        }
        g.insert(key, info);
    });
}
/// Read the delta result-size varint from a pack zlib stream without inflating the full delta.
///
/// Advances `*pos` past the compressed bytes consumed from `bytes`.
fn read_delta_result_size_from_pack_zlib(bytes: &[u8], pos: &mut usize) -> Result<u64> {
    DELTA_SIZE_ZLIB.with(|slot| slot.borrow_mut().read_pack_delta_result_size(bytes, pos))
}

fn decompress_pack_data_into(
    bytes: &[u8],
    pos: &mut usize,
    expected_size: u64,
    scratch: &mut ZlibInflateScratch,
    out: &mut Vec<u8>,
) -> Result<()> {
    scratch.decompress_fixed_into(bytes, pos, expected_size, out)
}

/// Read and fully resolve one object from a pack file given its offset.
///
/// Handles OFS_DELTA and REF_DELTA by recursively reading the base object.
/// The `idx` is used for REF_DELTA resolution (to find a base by OID).
/// Deepest delta chain a read will follow before treating the pack as corrupt.
///
/// Safety valve against cyclic ref-delta chains and unbounded recursion, not a
/// format limit — Git reads chains of any depth (`pack.depth` caps only writers).
const MAX_DELTA_CHAIN_READ_DEPTH: usize = 4096;

/// One hop in an in-flight delta chain (collected tip→base, applied base→tip).
#[derive(Clone)]
struct PendingDeltaFrame {
    pack_id: u32,
    offset: u64,
    delta: Vec<u8>,
}

/// Tracks delta-chain depth and the current `(pack_id, offset)` path for cycle detection.
///
/// Also holds reusable scratch buffers for pack reads on this thread (cleared per resolution).
#[derive(Default)]
struct DeltaChainState {
    depth: usize,
    visited: HashSet<(u32, u64)>,
    pending: Vec<PendingDeltaFrame>,
    inflate: ZlibInflateScratch,
    whole_object: Vec<u8>,
    delta_decompress: Vec<u8>,
    chain_a: Vec<u8>,
    chain_b: Vec<u8>,
}

// hygiene: pack read delta-chain state (ROADMAP item 7 — per-thread pack cache)
thread_local! {
    static PACK_READ_DELTA_STATE: std::cell::RefCell<DeltaChainState> =
        std::cell::RefCell::new(DeltaChainState::default());
}

fn with_pack_read_delta_state<R>(
    start_depth: usize,
    f: impl FnOnce(&mut DeltaChainState) -> R,
) -> R {
    PACK_READ_DELTA_STATE.with(|slot| {
        let mut state = slot.borrow_mut();
        state.depth = start_depth;
        state.visited.clear();
        f(&mut state)
    })
}

impl DeltaChainState {
    fn visit(&mut self, pack_id: u32, offset: u64) -> Result<()> {
        if self.depth > MAX_DELTA_CHAIN_READ_DEPTH {
            return Err(Error::DeltaChainTooDeep {
                limit: MAX_DELTA_CHAIN_READ_DEPTH,
            });
        }
        if !self.visited.insert((pack_id, offset)) {
            return Err(Error::DeltaChainTooDeep {
                limit: MAX_DELTA_CHAIN_READ_DEPTH,
            });
        }
        Ok(())
    }

    fn next_hop(&mut self) -> Result<()> {
        self.depth += 1;
        if self.depth > MAX_DELTA_CHAIN_READ_DEPTH {
            return Err(Error::DeltaChainTooDeep {
                limit: MAX_DELTA_CHAIN_READ_DEPTH,
            });
        }
        Ok(())
    }
}

/// Copy `payload` into `out`, reusing `out`'s capacity when sufficient.
fn copy_payload_preserve_capacity(out: &mut Vec<u8>, payload: &[u8]) {
    out.clear();
    out.extend_from_slice(payload);
}

/// Like [`apply_delta_chain_forward`], writing the final payload into `out` without replacing
/// `out`'s allocation when the result fits in the existing capacity.
#[allow(clippy::too_many_arguments)]
fn apply_delta_chain_forward_into(
    out: &mut Vec<u8>,
    kind: ObjectKind,
    base_data: &[u8],
    whole_base: Option<(u32, u64)>,
    pending: &[PendingDeltaFrame],
    result_pack_id: u32,
    result_offset: u64,
    populate_delta_lru: bool,
    state: &mut DeltaChainState,
) -> Result<()> {
    if pending.is_empty() {
        copy_payload_preserve_capacity(out, base_data);
        return Ok(());
    }

    let caching = populate_delta_lru && pack_cache::delta_base_caching_enabled();
    let mut cache_inserts: Vec<(u32, u64, Arc<[u8]>)> =
        Vec::with_capacity(if caching { pending.len() + 1 } else { 0 });
    if caching {
        if let Some((pid, off)) = whole_base {
            cache_inserts.push((pid, off, Arc::from(base_data.to_vec())));
        }
    }

    let chain_a = &mut state.chain_a;
    let chain_b = &mut state.chain_b;
    let mut rev: Vec<&PendingDeltaFrame> = pending.iter().rev().collect();
    let last = rev
        .pop()
        .ok_or_else(|| Error::CorruptObject("empty delta chain".to_owned()))?;
    if rev.is_empty() {
        apply_delta_into(out, base_data, &last.delta)?;
        if caching && (last.pack_id != result_pack_id || last.offset != result_offset) {
            cache_inserts.push((last.pack_id, last.offset, Arc::from(out.as_slice())));
        }
        if caching {
            pack_cache::put_delta_bases(kind, &cache_inserts);
        }
        return Ok(());
    }

    let Some(first) = rev.first().copied() else {
        return Err(Error::CorruptObject(
            "delta chain missing base-adjacent frame".to_owned(),
        ));
    };
    apply_delta_into(chain_a, base_data, &first.delta)?;
    if caching && (first.pack_id != result_pack_id || first.offset != result_offset) {
        cache_inserts.push((first.pack_id, first.offset, Arc::from(chain_a.as_slice())));
    }
    let mut src_is_a = true;
    for frame in rev.iter().skip(1) {
        if src_is_a {
            apply_delta_into(chain_b, chain_a.as_slice(), &frame.delta)?;
            if caching && (frame.pack_id != result_pack_id || frame.offset != result_offset) {
                cache_inserts.push((frame.pack_id, frame.offset, Arc::from(chain_b.as_slice())));
            }
        } else {
            apply_delta_into(chain_a, chain_b.as_slice(), &frame.delta)?;
            if caching && (frame.pack_id != result_pack_id || frame.offset != result_offset) {
                cache_inserts.push((frame.pack_id, frame.offset, Arc::from(chain_a.as_slice())));
            }
        }
        src_is_a = !src_is_a;
    }
    let penultimate_base = if src_is_a {
        chain_a.as_slice()
    } else {
        chain_b.as_slice()
    };
    apply_delta_into(out, penultimate_base, &last.delta)?;
    if caching && (last.pack_id != result_pack_id || last.offset != result_offset) {
        cache_inserts.push((last.pack_id, last.offset, Arc::from(out.as_slice())));
    }
    if caching {
        pack_cache::put_delta_bases(kind, &cache_inserts);
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn finish_pack_read_with_pending(
    out: &mut Vec<u8>,
    kind: ObjectKind,
    base: &[u8],
    whole_base: Option<(u32, u64)>,
    result_pack_id: u32,
    result_offset: u64,
    populate_delta_lru: bool,
    state: &mut DeltaChainState,
) -> Result<ObjectKind> {
    let pending = std::mem::take(&mut state.pending);
    apply_delta_chain_forward_into(
        out,
        kind,
        base,
        whole_base,
        &pending,
        result_pack_id,
        result_offset,
        populate_delta_lru,
        state,
    )?;
    Ok(kind)
}

/// Starting index for a pack read: borrow the caller's [`PackIndex`], or hold a cached
/// [`Arc`] after hopping to another pack (no deep clone of `entries`).
enum PackIndexHandle<'a> {
    Borrowed(&'a PackIndex),
    Shared(Arc<PackIndex>),
}

impl<'a> Clone for PackIndexHandle<'a> {
    fn clone(&self) -> Self {
        match self {
            Self::Borrowed(idx) => Self::Borrowed(idx),
            Self::Shared(arc) => Self::Shared(Arc::clone(arc)),
        }
    }
}

impl PackIndexHandle<'_> {
    fn get(&self) -> &PackIndex {
        match self {
            Self::Borrowed(idx) => idx,
            Self::Shared(idx) => idx.as_ref(),
        }
    }
}

fn find_other_pack_index(
    objects_dir: &Path,
    current: &PackIndex,
    oid: &ObjectId,
) -> Result<Option<Arc<PackIndex>>> {
    for idx in read_local_pack_indexes_cached(objects_dir)? {
        if idx.idx_path == current.idx_path {
            continue;
        }
        if idx.contains(oid)? {
            return Ok(Some(Arc::clone(&idx)));
        }
    }
    Ok(None)
}

/// When an indexed in-pack base is unreadable, try a verified loose copy or the same OID in
/// another pack (cold rescue path preserved from the recursive reader).
fn rescue_in_pack_base(
    idx: &PackIndex,
    base_offset: u64,
    objects_dir: Option<&Path>,
    state: &mut DeltaChainState,
) -> Result<Option<(ObjectKind, Vec<u8>)>> {
    let Some(pos) = idx.find_position_by_offset_sorted(base_offset)? else {
        return Ok(None);
    };
    let entry_oid = idx.oid_at(pos);
    if entry_oid.len() != idx.hash_bytes() {
        return Ok(None);
    }
    let base_oid = ObjectId::from_bytes(entry_oid)?;
    let Some(dir) = objects_dir else {
        return Ok(None);
    };
    let loose = dir
        .join(base_oid.loose_prefix())
        .join(base_oid.loose_suffix());
    if loose.is_file() {
        if let Ok(obj) = crate::odb::Odb::read_loose_verify_oid(&loose, &base_oid) {
            return Ok(Some((obj.kind, obj.data)));
        }
    }
    if let Some(other_idx) = find_other_pack_index(dir, idx, &base_oid)? {
        let Some(off) = other_idx.find_offset(&base_oid)? else {
            return Ok(None);
        };
        if let Ok((kind, data)) =
            resolve_pack_object_at(PackIndexHandle::Shared(other_idx), off, Some(dir), state)
        {
            return Ok(Some((kind, data)));
        }
    }
    Ok(None)
}

/// True when a pack parse failure might be stale cached bytes rather than on-disk corruption.
fn pack_bytes_parse_may_be_stale(err: &Error) -> bool {
    matches!(err, Error::CorruptObject(_) | Error::Zlib(_))
}

/// Resolve one pack object, walking delta chains iteratively across packs with shared
/// [`DeltaChainState`] so cycles and depth limits surface as [`Error::DeltaChainTooDeep`].
///
/// On corruption/zlib errors, revalidates the starting pack's on-disk signature once and
/// retries the full resolution before surfacing the error.
fn resolve_pack_object_at(
    start_idx: PackIndexHandle<'_>,
    start_offset: u64,
    objects_dir: Option<&Path>,
    state: &mut DeltaChainState,
) -> Result<(ObjectKind, Vec<u8>)> {
    resolve_pack_object_at_with_bytes(start_idx, start_offset, objects_dir, state, None)
}

fn resolve_pack_object_at_into(
    start_idx: PackIndexHandle<'_>,
    start_offset: u64,
    objects_dir: Option<&Path>,
    state: &mut DeltaChainState,
    populate_delta_lru: bool,
    out: &mut Vec<u8>,
) -> Result<ObjectKind> {
    resolve_pack_object_at_with_bytes_into(
        start_idx,
        start_offset,
        objects_dir,
        state,
        None,
        populate_delta_lru,
        out,
    )
}

fn resolve_pack_object_at_with_bytes(
    start_idx: PackIndexHandle<'_>,
    start_offset: u64,
    objects_dir: Option<&Path>,
    state: &mut DeltaChainState,
    pack_bytes_override: Option<&[u8]>,
) -> Result<(ObjectKind, Vec<u8>)> {
    let mut out = Vec::new();
    let kind = resolve_pack_object_at_with_bytes_into(
        start_idx,
        start_offset,
        objects_dir,
        state,
        pack_bytes_override,
        true,
        &mut out,
    )?;
    Ok((kind, out))
}

fn resolve_pack_object_at_with_bytes_into(
    start_idx: PackIndexHandle<'_>,
    start_offset: u64,
    objects_dir: Option<&Path>,
    state: &mut DeltaChainState,
    pack_bytes_override: Option<&[u8]>,
    populate_delta_lru: bool,
    out: &mut Vec<u8>,
) -> Result<ObjectKind> {
    let pack_path = start_idx.get().pack_path.clone();
    let start_pack_id = pack_cache::pack_id_for(&pack_path);
    for attempt in 0..2 {
        match resolve_pack_object_at_body(
            start_idx.clone(),
            start_offset,
            start_pack_id,
            objects_dir,
            state,
            pack_bytes_override,
            populate_delta_lru,
            out,
        ) {
            Ok(kind) => return Ok(kind),
            Err(e) if attempt == 0 && pack_bytes_parse_may_be_stale(&e) => {
                if pack_cache::reload_pack_bytes_after_parse_failure(&pack_path)? {
                    state.visited.retain(|(id, _)| *id != start_pack_id);
                    continue;
                }
                return Err(e);
            }
            Err(e) => return Err(e),
        }
    }
    unreachable!("at most two resolution attempts")
}

#[allow(clippy::too_many_arguments)]
fn resolve_pack_object_at_body(
    mut cur_idx: PackIndexHandle<'_>,
    start_offset: u64,
    result_pack_id: u32,
    objects_dir: Option<&Path>,
    state: &mut DeltaChainState,
    pack_bytes_override: Option<&[u8]>,
    populate_delta_lru: bool,
    out: &mut Vec<u8>,
) -> Result<ObjectKind> {
    let result_offset = start_offset;
    let mut cur_offset = start_offset;
    state.pending.clear();

    let mut cur_pack_path = cur_idx.get().pack_path.clone();
    let mut cur_pack_id = pack_cache::pack_id_for(&cur_pack_path);
    let mut validated_pack_count = false;

    enum PackBytesSource<'a> {
        Borrowed(&'a [u8]),
        Cached(Arc<PackData>),
    }

    let mut pack_bytes = match pack_bytes_override {
        Some(b) => PackBytesSource::Borrowed(b),
        None => PackBytesSource::Cached(read_pack_bytes_cached(&cur_pack_path)?),
    };

    loop {
        let idx_ref = cur_idx.get();
        if idx_ref.pack_path != cur_pack_path {
            cur_pack_path = idx_ref.pack_path.clone();
            pack_bytes = PackBytesSource::Cached(read_pack_bytes_cached(&cur_pack_path)?);
            cur_pack_id = pack_cache::pack_id_for(&cur_pack_path);
            validated_pack_count = false;
        }
        let pack_id = cur_pack_id;
        state.visit(pack_id, cur_offset)?;

        let bytes: &[u8] = match &pack_bytes {
            PackBytesSource::Borrowed(b) => b,
            PackBytesSource::Cached(c) => c.as_ref(),
        };

        if !validated_pack_count {
            validate_pack_index_object_count(bytes, idx_ref)?;
            validated_pack_count = true;
        }

        let object_start = cur_offset;
        let mut pos = cur_offset as usize;
        let (packed_type, size) = match parse_pack_object_header(bytes, &mut pos) {
            Ok(v) => v,
            Err(err) => {
                if let Some((kind, data)) =
                    rescue_in_pack_base(idx_ref, cur_offset, objects_dir, state)?
                {
                    return finish_pack_read_with_pending(
                        out,
                        kind,
                        &data,
                        None,
                        result_pack_id,
                        result_offset,
                        populate_delta_lru,
                        state,
                    );
                }
                return Err(err);
            }
        };

        match packed_type {
            PackedType::Commit | PackedType::Tree | PackedType::Blob | PackedType::Tag => {
                let kind = packed_type_to_kind(packed_type)?;
                if state.pending.is_empty() {
                    if let Err(err) =
                        decompress_pack_data_into(bytes, &mut pos, size, &mut state.inflate, out)
                    {
                        if let Some((rescue_kind, rescued)) =
                            rescue_in_pack_base(idx_ref, cur_offset, objects_dir, state)?
                        {
                            return finish_pack_read_with_pending(
                                out,
                                rescue_kind,
                                &rescued,
                                None,
                                result_pack_id,
                                result_offset,
                                populate_delta_lru,
                                state,
                            );
                        }
                        return Err(err);
                    }
                    return Ok(kind);
                }
                state.whole_object.clear();
                if let Err(err) = decompress_pack_data_into(
                    bytes,
                    &mut pos,
                    size,
                    &mut state.inflate,
                    &mut state.whole_object,
                ) {
                    if let Some((rescue_kind, rescued)) =
                        rescue_in_pack_base(idx_ref, cur_offset, objects_dir, state)?
                    {
                        return finish_pack_read_with_pending(
                            out,
                            rescue_kind,
                            &rescued,
                            None,
                            result_pack_id,
                            result_offset,
                            populate_delta_lru,
                            state,
                        );
                    }
                    return Err(err);
                }
                let whole = std::mem::take(&mut state.whole_object);
                return finish_pack_read_with_pending(
                    out,
                    kind,
                    &whole,
                    Some((pack_id, cur_offset)),
                    result_pack_id,
                    result_offset,
                    populate_delta_lru,
                    state,
                );
            }
            PackedType::OfsDelta => {
                state.next_hop()?;
                let base_offset = parse_ofs_delta_base(bytes, &mut pos, object_start)?;
                state.delta_decompress.clear();
                decompress_pack_data_into(
                    bytes,
                    &mut pos,
                    size,
                    &mut state.inflate,
                    &mut state.delta_decompress,
                )?;
                state.pending.push(PendingDeltaFrame {
                    pack_id,
                    offset: object_start,
                    delta: std::mem::take(&mut state.delta_decompress),
                });
                if let Some((base_kind, base_data)) =
                    pack_cache::get_delta_base(pack_id, base_offset)
                {
                    return finish_pack_read_with_pending(
                        out,
                        base_kind,
                        base_data.as_ref(),
                        None,
                        result_pack_id,
                        result_offset,
                        populate_delta_lru,
                        state,
                    );
                }
                cur_offset = base_offset;
            }
            PackedType::RefDelta => {
                state.next_hop()?;
                let hb = idx_ref.hash_bytes();
                if pos + hb > bytes.len() {
                    return Err(Error::CorruptObject(
                        "truncated ref-delta base OID".to_owned(),
                    ));
                }
                let base_raw = bytes[pos..pos + hb].to_vec();
                pos += hb;
                state.delta_decompress.clear();
                decompress_pack_data_into(
                    bytes,
                    &mut pos,
                    size,
                    &mut state.inflate,
                    &mut state.delta_decompress,
                )?;
                state.pending.push(PendingDeltaFrame {
                    pack_id,
                    offset: object_start,
                    delta: std::mem::take(&mut state.delta_decompress),
                });

                if let Ok(base_oid) = ObjectId::from_bytes(base_raw.as_slice()) {
                    if let Some(base_offset) = idx_ref.find_offset(&base_oid)? {
                        if let Some((base_kind, base_data)) =
                            pack_cache::get_delta_base(pack_id, base_offset)
                        {
                            return finish_pack_read_with_pending(
                                out,
                                base_kind,
                                base_data.as_ref(),
                                None,
                                result_pack_id,
                                result_offset,
                                populate_delta_lru,
                                state,
                            );
                        }
                        cur_offset = base_offset;
                        continue;
                    }
                }

                if hb == 20 {
                    if let (Some(dir), Ok(base_oid)) =
                        (objects_dir, ObjectId::from_bytes(base_raw.as_slice()))
                    {
                        let loose = dir
                            .join(base_oid.loose_prefix())
                            .join(base_oid.loose_suffix());
                        if loose.is_file() {
                            if let Ok(obj) =
                                crate::odb::Odb::read_loose_verify_oid(&loose, &base_oid)
                            {
                                return finish_pack_read_with_pending(
                                    out,
                                    obj.kind,
                                    &obj.data,
                                    None,
                                    result_pack_id,
                                    result_offset,
                                    populate_delta_lru,
                                    state,
                                );
                            }
                        }
                        if let Some(other_idx) = find_other_pack_index(dir, idx_ref, &base_oid)? {
                            let Some(off) = other_idx.find_offset(&base_oid)? else {
                                return Err(Error::ObjectNotFound(base_oid.to_hex()));
                            };
                            cur_idx = PackIndexHandle::Shared(other_idx);
                            cur_offset = off;
                            continue;
                        }
                    }
                }
                return Err(Error::CorruptObject(format!(
                    "ref-delta base {} not found in pack",
                    oid_bytes_to_hex(&base_raw)
                )));
            }
        }
    }
}

/// Resolve kind and uncompressed size for one pack object without materializing its body.
fn resolve_pack_object_info_at(
    start_idx: PackIndexHandle<'_>,
    start_offset: u64,
    objects_dir: Option<&Path>,
    state: &mut DeltaChainState,
) -> Result<ObjectInfo> {
    resolve_pack_object_info_at_with_bytes(start_idx, start_offset, objects_dir, state, None)
}

fn resolve_pack_object_info_at_body(
    cur_idx: PackIndexHandle<'_>,
    start_offset: u64,
    objects_dir: Option<&Path>,
    state: &mut DeltaChainState,
    pack_bytes_override: Option<&[u8]>,
) -> Result<ObjectInfo> {
    let cached_pack;
    let cur_bytes: &[u8] = if let Some(b) = pack_bytes_override {
        b
    } else {
        cached_pack = read_pack_bytes_cached(&cur_idx.get().pack_path)?;
        cached_pack.as_ref()
    };
    let mut cur_offset = start_offset;
    let mut result_size: Option<u64> = None;

    loop {
        let idx_ref = cur_idx.get();
        let pack_id = pack_cache::pack_id_for(&idx_ref.pack_path);
        state.visit(pack_id, cur_offset)?;

        if let Some(base) = cached_pack_read_info(&idx_ref.pack_path, cur_offset) {
            if let Some(size) = result_size {
                return Ok(ObjectInfo {
                    kind: base.kind,
                    size,
                });
            }
            return Ok(base);
        }
        let object_start = cur_offset;
        let mut pos = cur_offset as usize;
        let (packed_type, header_size) = parse_pack_object_header(cur_bytes, &mut pos)?;

        match packed_type {
            PackedType::Commit | PackedType::Tree | PackedType::Blob | PackedType::Tag => {
                let kind = packed_type_to_kind(packed_type)?;
                remember_pack_read_info(
                    &idx_ref.pack_path,
                    cur_offset,
                    ObjectInfo {
                        kind,
                        size: header_size,
                    },
                );
                let size = result_size.unwrap_or(header_size);
                return Ok(ObjectInfo { kind, size });
            }
            PackedType::OfsDelta => {
                state.next_hop()?;
                let base_offset = parse_ofs_delta_base(cur_bytes, &mut pos, object_start)?;
                if result_size.is_none() {
                    result_size = Some(read_delta_result_size_from_pack_zlib(cur_bytes, &mut pos)?);
                }
                cur_offset = base_offset;
            }
            PackedType::RefDelta => {
                state.next_hop()?;
                let hb = idx_ref.hash_bytes();
                if pos + hb > cur_bytes.len() {
                    return Err(Error::CorruptObject(
                        "truncated ref-delta base OID".to_owned(),
                    ));
                }
                let base_raw = &cur_bytes[pos..pos + hb];
                pos += hb;
                if result_size.is_none() {
                    result_size = Some(read_delta_result_size_from_pack_zlib(cur_bytes, &mut pos)?);
                }

                let base_oid = ObjectId::from_bytes(base_raw)?;
                if let Some(base_offset) = idx_ref.find_offset(&base_oid)? {
                    cur_offset = base_offset;
                    continue;
                }

                if let Some(dir) = objects_dir {
                    let loose = dir
                        .join(base_oid.loose_prefix())
                        .join(base_oid.loose_suffix());
                    if loose.is_file() {
                        let info = crate::odb::read_loose_object_info(&loose)?;
                        let size = result_size.unwrap_or(info.size);
                        return Ok(ObjectInfo {
                            kind: info.kind,
                            size,
                        });
                    }
                    if let Some(other_idx) = find_other_pack_index(dir, idx_ref, &base_oid)? {
                        let Some(off) = other_idx.find_offset(&base_oid)? else {
                            return Err(Error::ObjectNotFound(base_oid.to_hex()));
                        };
                        let base_info = resolve_pack_object_info_at(
                            PackIndexHandle::Shared(other_idx),
                            off,
                            objects_dir,
                            state,
                        )?;
                        let size = result_size.unwrap_or(base_info.size);
                        return Ok(ObjectInfo {
                            kind: base_info.kind,
                            size,
                        });
                    }
                }
                return Err(Error::CorruptObject(format!(
                    "ref-delta base {} not found in pack",
                    oid_bytes_to_hex(base_raw)
                )));
            }
        }
    }
}

fn read_pack_object_at(
    pack_bytes: &[u8],
    start_offset: u64,
    idx: &PackIndex,
    objects_dir: Option<&Path>,
    start_depth: usize,
) -> Result<(ObjectKind, Vec<u8>)> {
    let mut state = DeltaChainState {
        depth: start_depth,
        visited: HashSet::new(),
        ..Default::default()
    };
    resolve_pack_object_at_with_bytes(
        PackIndexHandle::Borrowed(idx),
        start_offset,
        objects_dir,
        &mut state,
        Some(pack_bytes),
    )
}

/// Read an object from a pack file by its OID.
///
/// Searches the given pack index for the OID, then reads and decompresses
/// the object from the corresponding pack file, resolving delta chains.
///
/// # Errors
///
/// Returns [`Error::ObjectNotFound`] if the OID is not in this pack.
pub fn read_object_from_pack(idx: &PackIndex, oid: &ObjectId) -> Result<Object> {
    if crate::pack_store::PackStore::has_context() {
        return read_object_from_pack_at_depth(idx, oid, 0);
    }
    let store = crate::pack_store::PackStore::legacy_for_pack_path(&idx.pack_path);
    crate::pack_store::PackStore::with_context(store, || {
        read_object_from_pack_at_depth(idx, oid, 0)
    })
}

/// Like [`read_object_from_pack`], inflating into `buf` and reusing its capacity.
///
/// # Errors
///
/// Same as [`read_object_from_pack`].
pub fn read_object_from_pack_into(
    idx: &PackIndex,
    oid: &ObjectId,
    buf: &mut Vec<u8>,
) -> Result<ObjectKind> {
    let Some(offset) = idx.find_offset(oid)? else {
        return Err(Error::ObjectNotFound(oid.to_hex()));
    };
    if crate::pack_store::PackStore::has_context() {
        return read_object_at_depth_into(idx, offset, 0, false, buf);
    }
    let store = crate::pack_store::PackStore::legacy_for_pack_path(&idx.pack_path);
    crate::pack_store::PackStore::with_context(store, || {
        read_object_at_depth_into(idx, offset, 0, false, buf)
    })
}

/// Read [`ObjectInfo`] for `oid` stored in `idx`'s pack without inflating the full object body.
///
/// # Errors
///
/// Same as [`read_object_from_pack`], except no hash verification is performed on the payload.
pub fn read_object_info_from_pack(idx: &PackIndex, oid: &ObjectId) -> Result<ObjectInfo> {
    let Some(offset) = idx.find_offset(oid)? else {
        return Err(Error::ObjectNotFound(oid.to_hex()));
    };
    read_object_info_from_pack_at_offset(idx, offset)
}

/// Read [`ObjectInfo`] at a known pack offset (avoids a second index lookup in batch workloads).
///
/// # Errors
///
/// Same as [`read_object_info_from_pack`].
pub fn read_object_info_from_pack_at_offset(idx: &PackIndex, offset: u64) -> Result<ObjectInfo> {
    let pack_path = idx.pack_path.clone();
    for attempt in 0..2 {
        let pack_bytes = read_pack_bytes_cached(&pack_path)?;
        if !pack_cache::pack_object_count_validated(&pack_path) {
            match validate_pack_index_object_count(&pack_bytes, idx) {
                Ok(()) => pack_cache::mark_pack_object_count_validated(&pack_path),
                Err(e) if attempt == 0 && pack_bytes_parse_may_be_stale(&e) => {
                    if pack_cache::reload_pack_bytes_after_parse_failure(&pack_path)? {
                        continue;
                    }
                    return Err(e);
                }
                Err(e) => return Err(e),
            }
        }
        match read_object_info_from_pack_at_offset_with_bytes(idx, offset, pack_bytes.as_ref()) {
            Ok(info) => return Ok(info),
            Err(e) if attempt == 0 && pack_bytes_parse_may_be_stale(&e) => {
                if pack_cache::reload_pack_bytes_after_parse_failure(&pack_path)? {
                    continue;
                }
                return Err(e);
            }
            Err(e) => return Err(e),
        }
    }
    unreachable!("at most two read attempts")
}

fn read_object_info_from_pack_at_offset_with_bytes(
    idx: &PackIndex,
    offset: u64,
    pack_bytes: &[u8],
) -> Result<ObjectInfo> {
    if let Some(info) = read_whole_pack_entry_info_fast(idx, offset, pack_bytes) {
        return Ok(info);
    }
    let objects_dir = idx.pack_path.parent().and_then(Path::parent);
    with_pack_read_delta_state(0, |state| {
        resolve_pack_object_info_at_with_bytes(
            PackIndexHandle::Borrowed(idx),
            offset,
            objects_dir,
            state,
            Some(pack_bytes),
        )
    })
}

fn resolve_pack_object_info_at_with_bytes(
    start_idx: PackIndexHandle<'_>,
    start_offset: u64,
    objects_dir: Option<&Path>,
    state: &mut DeltaChainState,
    pack_bytes_override: Option<&[u8]>,
) -> Result<ObjectInfo> {
    let idx_ref = start_idx.get();
    let pack_path = idx_ref.pack_path.clone();
    let start_pack_id = pack_cache::pack_id_for(&pack_path);
    if let Some(info) = cached_pack_read_info(&pack_path, start_offset) {
        return Ok(info);
    }
    for attempt in 0..2 {
        match resolve_pack_object_info_at_body(
            start_idx.clone(),
            start_offset,
            objects_dir,
            state,
            pack_bytes_override,
        ) {
            Ok(v) => {
                remember_pack_read_info(&pack_path, start_offset, v);
                return Ok(v);
            }
            Err(e) if attempt == 0 && pack_bytes_parse_may_be_stale(&e) => {
                if pack_cache::reload_pack_bytes_after_parse_failure(&pack_path)? {
                    state.visited.retain(|(id, _)| *id != start_pack_id);
                    continue;
                }
                return Err(e);
            }
            Err(e) => return Err(e),
        }
    }
    unreachable!("at most two resolution attempts")
}

/// Header-only info for undeltified pack entries (batch-check hot path).
#[inline]
fn read_whole_pack_entry_info_fast(
    _idx: &PackIndex,
    offset: u64,
    pack_bytes: &[u8],
) -> Option<ObjectInfo> {
    let mut pos = offset as usize;
    if pos >= pack_bytes.len() {
        return None;
    }
    let (packed_type, header_size) = parse_pack_object_header(pack_bytes, &mut pos).ok()?;
    match packed_type {
        PackedType::Commit | PackedType::Tree | PackedType::Blob | PackedType::Tag => {
            let kind = packed_type_to_kind(packed_type).ok()?;
            Some(ObjectInfo {
                kind,
                size: header_size,
            })
        }
        PackedType::OfsDelta | PackedType::RefDelta => None,
    }
}

// hygiene: per-thread zlib scratch for whole pack entry reads (ROADMAP item 7 — pack read hot path)
thread_local! {
    static WHOLE_OBJECT_ZLIB: RefCell<ZlibInflateScratch> =
        RefCell::new(ZlibInflateScratch::default());
}

/// Max uncompressed bytes to inflate when only commit headers (e.g. author) are needed.
pub(crate) const COMMIT_HEADER_READ_PREFIX: usize = 512;

fn read_whole_pack_entry_object_fast(
    idx: &PackIndex,
    offset: u64,
    pack_bytes: &[u8],
) -> Option<Object> {
    let mut data = Vec::new();
    read_whole_pack_entry_object_into(idx, offset, pack_bytes, &mut data)
        .map(|kind| Object::new(kind, data))
}

fn read_whole_pack_entry_object_into(
    idx: &PackIndex,
    offset: u64,
    pack_bytes: &[u8],
    out: &mut Vec<u8>,
) -> Option<ObjectKind> {
    let pack_id = pack_cache::pack_id_for(&idx.pack_path);
    if let Some((kind, data)) = pack_cache::get_delta_base(pack_id, offset) {
        out.clear();
        out.extend_from_slice(data.as_ref());
        return Some(kind);
    }
    let mut pos = offset as usize;
    if pos >= pack_bytes.len() {
        return None;
    }
    let (packed_type, size) = parse_pack_object_header(pack_bytes, &mut pos).ok()?;
    match packed_type {
        PackedType::Commit | PackedType::Tree | PackedType::Blob | PackedType::Tag => {
            let kind = packed_type_to_kind(packed_type).ok()?;
            WHOLE_OBJECT_ZLIB.with(|slot| {
                let mut inflate = slot.borrow_mut();
                decompress_pack_data_into(pack_bytes, &mut pos, size, &mut inflate, out)
                    .ok()
                    .map(|()| kind)
            })
        }
        PackedType::OfsDelta | PackedType::RefDelta => None,
    }
}

fn read_object_from_packs_in_context_into(
    objects_dir: &Path,
    oid: &ObjectId,
    buf: &mut Vec<u8>,
) -> Result<ObjectKind> {
    match try_read_object_from_packs_with_options_into(
        objects_dir,
        oid,
        PackLookupOptions::ALL_LOCAL_PACKS,
        buf,
    ) {
        Ok(kind) => Ok(kind),
        Err(err @ Error::ObjectNotFound(_)) => {
            if reprepare_pack_directory_on_miss(objects_dir)? {
                try_read_object_from_packs_with_options_into(
                    objects_dir,
                    oid,
                    PackLookupOptions::ALL_LOCAL_PACKS,
                    buf,
                )
            } else {
                Err(err)
            }
        }
        Err(err) => Err(err),
    }
}

fn read_object_from_packs_in_context(objects_dir: &Path, oid: &ObjectId) -> Result<Object> {
    match try_read_object_from_packs_with_options(
        objects_dir,
        oid,
        PackLookupOptions::ALL_LOCAL_PACKS,
    ) {
        Ok(obj) => Ok(obj),
        Err(err @ Error::ObjectNotFound(_)) => {
            if reprepare_pack_directory_on_miss_in_context(objects_dir)? {
                try_read_object_from_packs_with_options(
                    objects_dir,
                    oid,
                    PackLookupOptions::ALL_LOCAL_PACKS,
                )
            } else {
                Err(err)
            }
        }
        Err(err) => Err(err),
    }
}

/// Read and decompress the object stored at `offset` in `idx`'s pack file.
///
/// The pack index is used for pack path, object count validation, and resolving ref-delta
/// bases; the starting location comes from the caller (e.g. a multi-pack-index entry).
pub(crate) fn read_object_at(idx: &PackIndex, offset: u64) -> Result<Object> {
    read_object_at_depth(idx, offset, 0)
}

/// Read a packed object when the pack index entry is already known (batch cat-file workloads).
///
/// Avoids a second index lookup when iterating objects in offset order.
pub fn read_object_from_pack_at_offset(idx: &PackIndex, offset: u64) -> Result<Object> {
    read_object_at(idx, offset)
}

/// [`read_object_from_pack`] with an explicit starting delta-chain depth, used when the read
/// itself resolves a delta base from another pack (the chain budget must carry across packs).
fn read_object_from_pack_at_depth(idx: &PackIndex, oid: &ObjectId, depth: usize) -> Result<Object> {
    let Some(offset) = idx.find_offset(oid)? else {
        return Err(Error::ObjectNotFound(oid.to_hex()));
    };
    read_object_at_depth(idx, offset, depth)
}

fn read_object_at_depth(idx: &PackIndex, offset: u64, depth: usize) -> Result<Object> {
    let mut data = Vec::new();
    let kind = read_object_at_depth_into(idx, offset, depth, true, &mut data)?;
    Ok(Object::new(kind, data))
}

fn read_object_at_depth_into(
    idx: &PackIndex,
    offset: u64,
    depth: usize,
    populate_delta_lru: bool,
    out: &mut Vec<u8>,
) -> Result<ObjectKind> {
    if depth == 0 {
        let pack_id = pack_cache::pack_id_for(&idx.pack_path);
        if let Some((kind, data)) = pack_cache::get_delta_base(pack_id, offset) {
            out.clear();
            out.extend_from_slice(data.as_ref());
            return Ok(kind);
        }
    }
    let pack_path = &idx.pack_path;
    let objects_dir = idx.pack_path.parent().and_then(Path::parent);
    for attempt in 0..2 {
        let pack_bytes = read_pack_bytes_cached(pack_path)?;
        if !pack_cache::pack_object_count_validated(pack_path) {
            match validate_pack_index_object_count(&pack_bytes, idx) {
                Ok(()) => pack_cache::mark_pack_object_count_validated(pack_path),
                Err(e) if attempt == 0 && pack_bytes_parse_may_be_stale(&e) => {
                    if pack_cache::reload_pack_bytes_after_parse_failure(pack_path)? {
                        continue;
                    }
                    return Err(e);
                }
                Err(e) => return Err(e),
            }
        }
        if depth == 0 {
            if let Some(kind) = read_whole_pack_entry_object_into(idx, offset, &pack_bytes, out) {
                return Ok(kind);
            }
        }
        let resolved = with_pack_read_delta_state(depth, |state| {
            resolve_pack_object_at_into(
                PackIndexHandle::Borrowed(idx),
                offset,
                objects_dir,
                state,
                populate_delta_lru,
                out,
            )
        });
        match resolved {
            Ok(kind) => return Ok(kind),
            Err(e) if attempt == 0 && pack_bytes_parse_may_be_stale(&e) => {
                if pack_cache::reload_pack_bytes_after_parse_failure(pack_path)? {
                    continue;
                }
                return Err(e);
            }
            Err(e) => return Err(e),
        }
    }
    unreachable!("at most two read attempts")
}

/// Resolve an object from already-loaded pack bytes (used by `verify-pack`).
pub fn read_object_from_pack_bytes(
    pack_bytes: &[u8],
    idx: &PackIndex,
    oid: &[u8],
) -> Result<Object> {
    validate_pack_index_object_count(pack_bytes, idx)?;
    let id = ObjectId::from_bytes(oid).map_err(|_| Error::ObjectNotFound(oid_bytes_to_hex(oid)))?;
    let entry_offset = idx
        .find_offset(&id)?
        .ok_or_else(|| Error::ObjectNotFound(oid_bytes_to_hex(oid)))?;
    let store = crate::pack_store::PackStore::legacy_for_pack_path(&idx.pack_path);
    crate::pack_store::PackStore::with_context(store, || {
        let (kind, data) = read_pack_object_at(pack_bytes, entry_offset, idx, None, 0)?;
        verify_packed_object_hash(kind, &data, oid)?;
        Ok(Object::new(kind, data))
    })
}

fn validate_pack_index_object_count(pack_bytes: &[u8], idx: &PackIndex) -> Result<()> {
    if pack_bytes.len() < 12 || &pack_bytes[0..4] != b"PACK" {
        return Err(Error::CorruptObject("bad pack header".to_owned()));
    }
    let count =
        u32::from_be_bytes([pack_bytes[8], pack_bytes[9], pack_bytes[10], pack_bytes[11]]) as usize;
    if count != idx.len() {
        return Err(Error::CorruptObject(format!(
            "pack object count mismatch: pack has {count}, index has {}",
            idx.len()
        )));
    }
    Ok(())
}

fn verify_packed_object_hash(kind: ObjectKind, data: &[u8], expected_oid: &[u8]) -> Result<()> {
    let algo = HashAlgo::from_len(expected_oid.len()).ok_or_else(|| {
        Error::CorruptObject(format!(
            "unsupported packed object id width: {}",
            expected_oid.len()
        ))
    })?;
    let actual = hash_object(algo, kind, data)?;
    if actual.as_bytes() != expected_oid {
        return Err(Error::CorruptObject(format!(
            "packed object {} hashes to {}",
            oid_bytes_to_hex(expected_oid),
            actual.to_hex()
        )));
    }
    Ok(())
}

/// Options controlling which local packs participate in a lookup pass.
#[derive(Clone, Copy, Debug, Default)]
pub struct PackLookupOptions {
    /// When true, skip pack indexes named in the active multi-pack-index (MIDX lookup runs separately).
    pub skip_midx_covered_packs: bool,
    /// When true, search only pack indexes named in the active MIDX (rescue pass after a MIDX miss).
    pub only_midx_covered_packs: bool,
}

impl PackLookupOptions {
    /// Search every local pack (including MIDX-covered indexes).
    pub const ALL_LOCAL_PACKS: Self = Self {
        skip_midx_covered_packs: false,
        only_midx_covered_packs: false,
    };

    /// Search only MIDX-listed pack indexes (Git redundant-pack rescue when MIDX lookup missed).
    pub const MIDX_COVERED_PACKS: Self = Self {
        skip_midx_covered_packs: false,
        only_midx_covered_packs: true,
    };

    /// Whether the single-pack and last-hit-pack shortcuts may run before the full scan.
    ///
    /// Those shortcuts probe a pack without consulting the MIDX filter, so a filtered pass
    /// would re-probe packs the other pass owns (every loose-object read pays both passes).
    fn allows_pack_shortcuts(self) -> bool {
        !self.skip_midx_covered_packs && !self.only_midx_covered_packs
    }
}

/// Pick one pack index for monotonic commit reads (single pack, or commit pack + blob promisor).
fn select_single_local_pack_index(
    indexes: &[Arc<PackIndex>],
    commit_hint: Option<&crate::objects::ObjectId>,
) -> Result<Option<Arc<PackIndex>>> {
    if indexes.is_empty() {
        return Ok(None);
    }
    if indexes.len() == 1 {
        return Ok(Some(Arc::clone(&indexes[0])));
    }
    if let Some(oid) = commit_hint {
        let mut found: Option<Arc<PackIndex>> = None;
        for idx in indexes {
            if idx.find_offset(oid)?.is_some() {
                if found.is_some() {
                    return Ok(None);
                }
                found = Some(Arc::clone(idx));
            }
        }
        if found.is_some() {
            return Ok(found);
        }
    }
    Ok(None)
}

/// Sorted `cat-file --batch` reader for repos with exactly one local pack (hot-path benchmarks).
///
/// Keeps a pack index cursor so probes in non-decreasing OID order avoid repeated binary search.
pub struct SingleLocalPackReader {
    idx: Arc<PackIndex>,
    pack_bytes: Arc<PackData>,
    pack_id: u32,
    cursor: usize,
    info_cache: HashMap<u64, ObjectInfo>,
    /// Created on first header/size probe; full object reads never touch it, so opening
    /// a reader per [`crate::odb::Odb::read`] must not pay for an inflater.
    delta_scratch: Option<ZlibInflateScratch>,
    commit_header_buf: Vec<u8>,
}

fn objects_dir_has_loose_objects(objects_dir: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(objects_dir) else {
        return false;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.len() != 2 || !name.bytes().all(|b| b.is_ascii_hexdigit()) {
            continue;
        }
        let Ok(hex_entries) = std::fs::read_dir(entry.path()) else {
            continue;
        };
        if hex_entries.flatten().any(|e| e.path().is_file()) {
            return true;
        }
    }
    false
}

impl SingleLocalPackReader {
    /// Open when [`crate::pack_store::PackStore`] read context matches and one local pack index exists.
    ///
    /// # Errors
    ///
    /// Propagates pack directory read failures from index loading.
    pub fn open(objects_dir: &Path) -> Result<Option<Self>> {
        Self::open_with_options(objects_dir, false, None)
    }

    /// Open when one local pack exists, optionally ignoring loose objects (commit parent walks).
    pub fn open_allow_loose(objects_dir: &Path) -> Result<Option<Self>> {
        Self::open_with_options(objects_dir, true, None)
    }

    /// Like [`Self::open_allow_loose`], but when several packs exist (e.g. promisor blob
    /// packs from partial clone), select the pack that contains `commit_hint`.
    pub(crate) fn open_allow_loose_for_commits(
        objects_dir: &Path,
        commit_hint: &crate::objects::ObjectId,
    ) -> Result<Option<Self>> {
        Self::open_with_options(objects_dir, true, Some(commit_hint))
    }

    /// Open a reader bound to a specific on-disk pack index (bitmap / pack-order walks).
    pub(crate) fn open_for_pack_index(
        objects_dir: &Path,
        idx: Arc<PackIndex>,
    ) -> Result<Option<Self>> {
        if !crate::pack_store::PackStore::read_context_matches_objects_dir(objects_dir) {
            return Ok(None);
        }
        pack_cache::sync_thread_local_with_store(&crate::pack_store::PackStore::active_for_reads());
        let pack_bytes = read_pack_bytes_cached(&idx.pack_path)?;
        validate_pack_index_object_count(pack_bytes.as_ref(), idx.as_ref())?;
        pack_cache::mark_pack_object_count_validated(&idx.pack_path);
        pack_cache::remember_pack_hit(objects_dir, &idx);
        let pack_id = pack_cache::pack_id_for(&idx.pack_path);
        Ok(Some(Self {
            idx,
            pack_bytes,
            pack_id,
            cursor: 0,
            info_cache: HashMap::new(),
            delta_scratch: None,
            commit_header_buf: Vec::new(),
        }))
    }

    /// Object kind of every entry starting at `offsets` (ascending), from entry headers alone.
    ///
    /// A delta takes the kind of its base, so nothing is inflated. The result is `None` for a
    /// delta whose base is not an earlier entry of `offsets` (a REF_DELTA base stored later in
    /// the pack or outside it), and for deltas on such entries; callers resolve those another way.
    ///
    /// # Errors
    ///
    /// [`Error::CorruptObject`] when an offset lies outside the pack or an entry header is
    /// malformed.
    pub(crate) fn object_kinds_at_sorted_offsets(
        &self,
        offsets: &[u64],
    ) -> Result<Vec<Option<ObjectKind>>> {
        let pack_bytes = self.pack_bytes.as_ref();
        let hash_bytes = self.idx.hash_bytes();
        let mut kinds: Vec<Option<ObjectKind>> = Vec::with_capacity(offsets.len());
        for (rank, &offset) in offsets.iter().enumerate() {
            let mut pos = usize::try_from(offset)
                .map_err(|_| Error::CorruptObject("pack offset out of range".to_owned()))?;
            let (packed_type, _) = parse_pack_object_header(pack_bytes, &mut pos)?;
            let base_offset = match packed_type {
                PackedType::OfsDelta => Some(parse_ofs_delta_base(pack_bytes, &mut pos, offset)?),
                PackedType::RefDelta => {
                    let raw = pack_bytes.get(pos..pos + hash_bytes).ok_or_else(|| {
                        Error::CorruptObject("truncated ref-delta base OID".to_owned())
                    })?;
                    self.idx.find_offset(&ObjectId::from_bytes(raw)?)?
                }
                _ => {
                    kinds.push(Some(packed_type_to_kind(packed_type)?));
                    continue;
                }
            };
            let base_kind = base_offset
                .and_then(|base| offsets.binary_search(&base).ok())
                .filter(|&base_rank| base_rank < rank)
                .and_then(|base_rank| kinds[base_rank]);
            kinds.push(base_kind);
        }
        Ok(kinds)
    }

    /// Whether `oid` is indexed in this reader's pack.
    pub(crate) fn contains_oid(&self, oid: &crate::objects::ObjectId) -> Result<bool> {
        Ok(self.idx.find_offset(oid)?.is_some())
    }

    fn open_with_options(
        objects_dir: &Path,
        allow_loose: bool,
        commit_hint: Option<&crate::objects::ObjectId>,
    ) -> Result<Option<Self>> {
        if !crate::pack_store::PackStore::read_context_matches_objects_dir(objects_dir) {
            return Ok(None);
        }
        pack_cache::sync_thread_local_with_store(&crate::pack_store::PackStore::active_for_reads());
        let memo_eligible = allow_loose && commit_hint.is_none();
        if memo_eligible {
            if let Some((idx, pack_bytes, pack_id)) = pack_cache::single_reader_for(objects_dir) {
                return Ok(Some(Self {
                    idx,
                    pack_bytes,
                    pack_id,
                    cursor: 0,
                    info_cache: HashMap::new(),
                    delta_scratch: None,
                    commit_header_buf: Vec::new(),
                }));
            }
        }
        let indexes = pack_indexes_for_lookup(objects_dir, PackLookupOptions::ALL_LOCAL_PACKS)?;
        let idx = match select_single_local_pack_index(&indexes, commit_hint)? {
            Some(idx) => idx,
            None => return Ok(None),
        };
        if !allow_loose && objects_dir_has_loose_objects(objects_dir) {
            return Ok(None);
        }
        let pack_bytes = read_pack_bytes_cached(&idx.pack_path)?;
        validate_pack_index_object_count(pack_bytes.as_ref(), idx.as_ref())?;
        pack_cache::mark_pack_object_count_validated(&idx.pack_path);
        pack_cache::remember_pack_hit(objects_dir, &idx);
        let pack_id = pack_cache::pack_id_for(&idx.pack_path);
        if memo_eligible && indexes.len() == 1 {
            pack_cache::remember_single_reader(objects_dir, &idx, &pack_bytes, pack_id);
        }
        Ok(Some(Self {
            idx,
            pack_bytes,
            pack_id,
            cursor: 0,
            info_cache: HashMap::new(),
            delta_scratch: None,
            commit_header_buf: Vec::new(),
        }))
    }

    /// Inflate the start of commit `oid` into an internal buffer (undeltified entries only).
    ///
    /// Returns `None` when the object is missing, not a commit, or stored as a delta.
    ///
    /// # Errors
    ///
    /// Propagates pack index and zlib errors.
    pub fn read_commit_author_prefix(&mut self, oid: &ObjectId) -> Result<Option<&[u8]>> {
        self.read_commit_author_prefix_at(oid, true)
    }

    /// Like [`Self::read_commit_author_prefix`], but pack index probes may be out of OID order.
    pub(crate) fn read_commit_author_prefix_unordered(
        &mut self,
        oid: &ObjectId,
    ) -> Result<Option<&[u8]>> {
        self.read_commit_author_prefix_at(oid, false)
    }

    /// Inflate the start of the commit at pack index row `row` (pack-order / bitmap walks).
    pub(crate) fn read_commit_author_prefix_at_pack_row(
        &mut self,
        row: usize,
    ) -> Result<Option<&[u8]>> {
        self.cursor = row;
        let offset = self.idx.offset_at(row)?;
        self.read_commit_author_prefix_at_pack_offset(offset)
    }

    fn read_commit_author_prefix_at(
        &mut self,
        oid: &ObjectId,
        monotonic: bool,
    ) -> Result<Option<&[u8]>> {
        let offset = if monotonic {
            self.idx.find_offset_monotonic(oid, &mut self.cursor)?
        } else {
            self.idx.find_offset(oid)?
        };
        let Some(offset) = offset else {
            return Ok(None);
        };
        self.read_commit_author_prefix_at_pack_offset(offset)
    }

    fn read_commit_author_prefix_at_pack_offset(&mut self, offset: u64) -> Result<Option<&[u8]>> {
        let pack_bytes = self.pack_bytes.as_ref();
        let mut pos = offset as usize;
        if pos >= pack_bytes.len() {
            return Ok(None);
        }
        let (packed_type, _) = match parse_pack_object_header(pack_bytes, &mut pos) {
            Ok(v) => v,
            Err(_) => return Ok(None),
        };
        if !matches!(packed_type, PackedType::Commit) {
            return Ok(None);
        }
        self.commit_header_buf.clear();
        self.delta_scratch
            .get_or_insert_with(ZlibInflateScratch::default)
            .decompress_prefix_into(
                &pack_bytes[pos..],
                COMMIT_HEADER_READ_PREFIX,
                &mut self.commit_header_buf,
            )?;
        if self.commit_header_buf.is_empty() {
            return Ok(None);
        }
        Ok(Some(&self.commit_header_buf))
    }

    /// Drop any cached commit header prefix (required before delta / ODB fallbacks).
    pub(crate) fn clear_commit_header_prefix(&mut self) {
        self.commit_header_buf.clear();
    }

    fn read_info_at_pack_offset(&mut self, start_offset: u64) -> Result<ObjectInfo> {
        if let Some(info) = self.info_cache.get(&start_offset) {
            return Ok(*info);
        }
        let pack_path = self.idx.pack_path.clone();
        let info = with_pack_read_delta_state(0, |state| {
            self.resolve_info_at_pack_offset_body(start_offset, state)
        })?;
        self.info_cache.insert(start_offset, info);
        remember_pack_read_info(&pack_path, start_offset, info);
        Ok(info)
    }

    fn resolve_info_at_pack_offset_body(
        &mut self,
        start_offset: u64,
        state: &mut DeltaChainState,
    ) -> Result<ObjectInfo> {
        let pack_bytes = self.pack_bytes.as_ref();
        let mut cur_offset = start_offset;
        let mut result_size: Option<u64> = None;
        loop {
            state.visit(self.pack_id, cur_offset)?;
            if let Some(base) = self.info_cache.get(&cur_offset) {
                if let Some(size) = result_size {
                    return Ok(ObjectInfo {
                        kind: base.kind,
                        size,
                    });
                }
                return Ok(*base);
            }

            let object_start = cur_offset;
            let mut pos = cur_offset as usize;
            let (packed_type, header_size) = parse_pack_object_header(pack_bytes, &mut pos)?;
            match packed_type {
                PackedType::Commit | PackedType::Tree | PackedType::Blob | PackedType::Tag => {
                    let kind = packed_type_to_kind(packed_type)?;
                    let undeltified = ObjectInfo {
                        kind,
                        size: header_size,
                    };
                    self.info_cache.insert(cur_offset, undeltified);
                    let size = result_size.unwrap_or(header_size);
                    return Ok(ObjectInfo { kind, size });
                }
                PackedType::OfsDelta => {
                    state.next_hop()?;
                    let base_offset = parse_ofs_delta_base(pack_bytes, &mut pos, object_start)?;
                    if result_size.is_none() {
                        result_size = Some(
                            self.delta_scratch
                                .get_or_insert_with(ZlibInflateScratch::default)
                                .read_pack_delta_result_size(pack_bytes, &mut pos)?,
                        );
                    }
                    cur_offset = base_offset;
                }
                PackedType::RefDelta => {
                    state.next_hop()?;
                    let hb = self.idx.hash_bytes();
                    if pos + hb > pack_bytes.len() {
                        return Err(Error::CorruptObject(
                            "truncated ref-delta base OID".to_owned(),
                        ));
                    }
                    let base_raw = &pack_bytes[pos..pos + hb];
                    pos += hb;
                    if result_size.is_none() {
                        result_size = Some(
                            self.delta_scratch
                                .get_or_insert_with(ZlibInflateScratch::default)
                                .read_pack_delta_result_size(pack_bytes, &mut pos)?,
                        );
                    }
                    let base_oid = ObjectId::from_bytes(base_raw)?;
                    let Some(base_offset) = self.idx.find_offset(&base_oid)? else {
                        return Err(Error::ObjectNotFound(base_oid.to_hex()));
                    };
                    cur_offset = base_offset;
                }
            }
        }
    }

    /// Kind and size for `oid` without loading the object body.
    ///
    /// # Errors
    ///
    /// Same as [`read_object_info_from_pack_at_offset`].
    pub fn read_info(&mut self, oid: &ObjectId) -> Result<ObjectInfo> {
        let Some(offset) = self.idx.find_offset_monotonic(oid, &mut self.cursor)? else {
            return Err(Error::ObjectNotFound(oid.to_hex()));
        };
        if let Some(info) =
            read_whole_pack_entry_info_fast(&self.idx, offset, self.pack_bytes.as_ref())
        {
            self.info_cache.insert(offset, info);
            return Ok(info);
        }
        self.read_info_at_pack_offset(offset)
    }

    /// Decompress the object for `oid`.
    ///
    /// # Errors
    ///
    /// Same as [`read_object_from_pack_at_offset`].
    pub fn read_object(&mut self, oid: &ObjectId) -> Result<Object> {
        let Some(offset) = self.idx.find_offset_monotonic(oid, &mut self.cursor)? else {
            return Err(Error::ObjectNotFound(oid.to_hex()));
        };
        if let Some(obj) =
            read_whole_pack_entry_object_fast(&self.idx, offset, self.pack_bytes.as_ref())
        {
            return Ok(obj);
        }
        read_object_from_pack_at_offset(&self.idx, offset)
    }

    /// Like [`Self::read_object`], inflating into `out` and preserving its capacity when possible.
    pub fn read_object_into(&mut self, oid: &ObjectId, out: &mut Vec<u8>) -> Result<ObjectKind> {
        let Some(offset) = self.idx.find_offset_monotonic(oid, &mut self.cursor)? else {
            return Err(Error::ObjectNotFound(oid.to_hex()));
        };
        if let Some(kind) =
            read_whole_pack_entry_object_into(&self.idx, offset, self.pack_bytes.as_ref(), out)
        {
            return Ok(kind);
        }
        read_object_at_depth_into(&self.idx, offset, 0, false, out)
    }

    /// Pack file offset for `oid` in this reader's index, if present.
    ///
    /// # Errors
    ///
    /// Propagates pack index lookup errors.
    pub fn pack_offset(&self, oid: &ObjectId) -> Result<Option<u64>> {
        self.idx.find_offset(oid)
    }
}
fn pack_indexes_for_lookup(
    objects_dir: &Path,
    opts: PackLookupOptions,
) -> Result<Vec<Arc<PackIndex>>> {
    let indexes = read_local_pack_indexes_cached(objects_dir)?;
    if !opts.skip_midx_covered_packs && !opts.only_midx_covered_packs {
        return Ok(indexes);
    }
    let covered = pack_cache::midx_covered_pack_names(objects_dir);
    if covered.is_empty() {
        return if opts.only_midx_covered_packs {
            Ok(Vec::new())
        } else {
            Ok(indexes)
        };
    }
    Ok(indexes
        .into_iter()
        .filter(|idx| {
            idx.idx_path
                .file_name()
                .and_then(|n| n.to_str())
                .map(|name| {
                    if opts.only_midx_covered_packs {
                        covered.contains(name)
                    } else {
                        !covered.contains(name)
                    }
                })
                .unwrap_or(!opts.only_midx_covered_packs)
        })
        .collect())
}

/// Search all pack indexes in `objects_dir` for the given OID and read it.
///
/// When more than one pack contains `oid` (a redundant copy), a read failure in
/// one pack — e.g. a corrupted delta base or zlib stream — is not fatal: Git
/// retries the remaining sources before giving up, so an intact redundant pack
/// still satisfies the read (t5303 pack-corruption-resilience). Only when every
/// pack that names `oid` fails to produce it do we surface the last error.
///
/// # Errors
///
/// Returns [`Error::ObjectNotFound`] if no pack contains the OID.
pub fn read_object_from_packs(objects_dir: &Path, oid: &ObjectId) -> Result<Object> {
    if crate::pack_store::PackStore::has_context() {
        return read_object_from_packs_in_context(objects_dir, oid);
    }
    let store = crate::pack_store::PackStore::legacy_for_objects_dir(objects_dir);
    crate::pack_store::PackStore::with_context(store, || {
        read_object_from_packs_in_context(objects_dir, oid)
    })
}

/// Like [`read_object_from_packs`], inflating into `buf` and reusing its capacity.
///
/// # Errors
///
/// Same as [`read_object_from_packs`].
pub fn read_object_from_packs_into(
    objects_dir: &Path,
    oid: &ObjectId,
    buf: &mut Vec<u8>,
) -> Result<ObjectKind> {
    if crate::pack_store::PackStore::has_context() {
        return read_object_from_packs_in_context_into(objects_dir, oid, buf);
    }
    let store = crate::pack_store::PackStore::legacy_for_objects_dir(objects_dir);
    crate::pack_store::PackStore::with_context(store, || {
        read_object_from_packs_in_context_into(objects_dir, oid, buf)
    })
}

/// Search local packs for `oid` and return its kind and uncompressed size.
///
/// # Errors
///
/// Returns [`Error::ObjectNotFound`] when no pack lists `oid`.
pub fn read_object_info_from_packs(objects_dir: &Path, oid: &ObjectId) -> Result<ObjectInfo> {
    match try_read_object_info_from_packs_with_options(
        objects_dir,
        oid,
        PackLookupOptions::ALL_LOCAL_PACKS,
    ) {
        Ok(info) => Ok(info),
        Err(err @ Error::ObjectNotFound(_)) => {
            if reprepare_pack_directory_on_miss(objects_dir)? {
                try_read_object_info_from_packs_with_options(
                    objects_dir,
                    oid,
                    PackLookupOptions::ALL_LOCAL_PACKS,
                )
            } else {
                Err(err)
            }
        }
        Err(err) => Err(err),
    }
}

/// Search local packs for `oid` without re-preparing the pack directory listing.
pub(crate) fn try_read_object_info_from_packs_with_options(
    objects_dir: &Path,
    oid: &ObjectId,
    opts: PackLookupOptions,
) -> Result<ObjectInfo> {
    if crate::pack_store::PackStore::read_context_matches_objects_dir(objects_dir) {
        pack_cache::sync_thread_local_with_store(&crate::pack_store::PackStore::active_for_reads());
    }
    if opts.allows_pack_shortcuts() {
        if let Some(info) = pack_cache::try_read_info_from_hinted_pack(objects_dir, oid)? {
            return Ok(info);
        }
    }
    let indexes = pack_indexes_for_lookup(objects_dir, opts)?;
    let mut last_err: Option<Error> = None;
    for idx in &indexes {
        let Some(offset) = idx.find_offset(oid)? else {
            continue;
        };
        match read_object_info_from_pack_at_offset(idx, offset) {
            Ok(info) => {
                pack_cache::remember_pack_hit(objects_dir, idx);
                return Ok(info);
            }
            Err(Error::ObjectNotFound(_)) => {}
            Err(err) => last_err = Some(err),
        }
    }
    Err(last_err.unwrap_or_else(|| Error::ObjectNotFound(oid.to_hex())))
}

/// Search local packs for `oid` without re-preparing the pack directory listing.
pub(crate) fn try_read_object_from_packs_with_options(
    objects_dir: &Path,
    oid: &ObjectId,
    opts: PackLookupOptions,
) -> Result<Object> {
    if crate::pack_store::PackStore::read_context_matches_objects_dir(objects_dir) {
        pack_cache::sync_thread_local_with_store(&crate::pack_store::PackStore::active_for_reads());
    }
    if opts.allows_pack_shortcuts() {
        if let Some(obj) = pack_cache::try_read_object_via_single_pack_reader(objects_dir, oid)? {
            return Ok(obj);
        }
        if let Some(obj) = pack_cache::try_read_from_hinted_pack(objects_dir, oid)? {
            return Ok(obj);
        }
    }
    let indexes = pack_indexes_for_lookup(objects_dir, opts)?;
    let mut last_err: Option<Error> = None;
    for idx in &indexes {
        let Some(offset) = pack_cache::find_offset_in_pack_read_context(objects_dir, idx, oid)?
        else {
            continue;
        };
        match read_object_from_pack_at_offset(idx, offset) {
            Ok(obj) => {
                pack_cache::remember_pack_hit(objects_dir, idx);
                return Ok(obj);
            }
            // The object is missing from this particular pack despite the index
            // claim — keep looking in the others.
            Err(Error::ObjectNotFound(_)) => {}
            // The pack copy is unreadable (corrupt delta/zlib/header). A redundant
            // pack may still hold an intact copy, so remember the error and retry.
            Err(err) => last_err = Some(err),
        }
    }
    Err(last_err.unwrap_or_else(|| Error::ObjectNotFound(oid.to_hex())))
}

/// Like [`try_read_object_from_packs_with_options`], writing the payload into `buf`.
pub(crate) fn try_read_object_from_packs_with_options_into(
    objects_dir: &Path,
    oid: &ObjectId,
    opts: PackLookupOptions,
    buf: &mut Vec<u8>,
) -> Result<ObjectKind> {
    if crate::pack_store::PackStore::read_context_matches_objects_dir(objects_dir) {
        pack_cache::sync_thread_local_with_store(&crate::pack_store::PackStore::active_for_reads());
    }
    if opts.allows_pack_shortcuts() {
        if let Some(kind) =
            pack_cache::try_read_object_via_single_pack_reader_into(objects_dir, oid, buf)?
        {
            return Ok(kind);
        }
        if let Some(kind) = pack_cache::try_read_from_hinted_pack_into(objects_dir, oid, buf)? {
            return Ok(kind);
        }
    }
    let indexes = pack_indexes_for_lookup(objects_dir, opts)?;
    let mut last_err: Option<Error> = None;
    for idx in &indexes {
        let Some(offset) = pack_cache::find_offset_in_pack_read_context(objects_dir, idx, oid)?
        else {
            continue;
        };
        match read_object_at_depth_into(idx, offset, 0, false, buf) {
            Ok(kind) => {
                pack_cache::remember_pack_hit(objects_dir, idx);
                return Ok(kind);
            }
            Err(Error::ObjectNotFound(_)) => {}
            Err(err) => last_err = Some(err),
        }
    }
    Err(last_err.unwrap_or_else(|| Error::ObjectNotFound(oid.to_hex())))
}

/// Whether `idx_path` uses the legacy v1 pack-index format (no `PACK` signature).
#[must_use]
pub fn pack_index_is_v1(idx_path: &Path) -> bool {
    fs::read(idx_path)
        .ok()
        .is_some_and(|bytes| bytes.len() >= 4 && bytes[0..4] != [0xff, b't', b'O', b'c'])
}

/// Resolve `oid` from local packs via the process-wide pack cache (cached `.idx`
/// parses, cached pack bytes, fanout binary search, delta-base cache).
///
/// Returns `Ok(None)` when no local pack index names the object. When a pack
/// copy fails to decode from a v1 (legacy) index, returns an empty blob placeholder
/// (historical pack-objects behavior). Any other decode failure invokes
/// `alternate_read` once before trying the next pack.
///
/// # Errors
///
/// Returns [`Error::Io`] when pack indexes cannot be enumerated.
pub fn try_read_object_from_local_packs_cached(
    objects_dir: &Path,
    oid: &ObjectId,
    mut alternate_read: impl FnMut() -> Result<Object>,
) -> Result<Option<Object>> {
    let indexes = read_local_pack_indexes_cached(objects_dir)?;
    for idx in &indexes {
        if idx.find_offset(oid)?.is_none() {
            continue;
        }
        match read_object_from_pack(idx, oid) {
            Ok(obj) => return Ok(Some(obj)),
            Err(_) if pack_index_is_v1(&idx.idx_path) => {
                return Ok(Some(Object::new(ObjectKind::Blob, Vec::new())));
            }
            Err(_) => {
                if let Ok(obj) = alternate_read() {
                    return Ok(Some(obj));
                }
            }
        }
    }
    Ok(None)
}

/// When `oid` is stored as a delta in a pack, return its delta base object id.
/// Returns [`None`] for loose objects and for non-delta packed objects.
/// Parse a packed delta entry and inflate its instruction stream when the base OID is in `in_pack`.
fn parse_in_pack_delta_reuse_candidate(
    idx: &PackIndex,
    pack_bytes: &[u8],
    entry_offset: u64,
    in_pack: &HashSet<ObjectId>,
) -> Result<Option<(ObjectId, Vec<u8>)>> {
    let hb = idx.hash_bytes();
    let mut p = entry_offset as usize;
    let (packed_type, size) = parse_pack_object_header(pack_bytes, &mut p)?;
    let base = match packed_type {
        PackedType::RefDelta => {
            if p + hb > pack_bytes.len() {
                return Err(Error::CorruptObject(
                    "truncated ref-delta base oid while scanning for reuse".to_owned(),
                ));
            }
            let bo = ObjectId::from_bytes(&pack_bytes[p..p + hb])?;
            p += hb;
            bo
        }
        PackedType::OfsDelta => {
            let base_off = parse_ofs_delta_base(pack_bytes, &mut p, entry_offset)?;
            let Some(pos) = idx.find_position_by_offset_sorted(base_off)? else {
                return Ok(None);
            };
            let base_oid_bytes = idx.oid_at(pos);
            if base_oid_bytes.len() != hb {
                return Ok(None);
            }
            ObjectId::from_bytes(base_oid_bytes)?
        }
        _ => return Ok(None),
    };
    if !in_pack.contains(&base) {
        return Ok(None);
    }
    let zlib_start = p;
    let Some(end_pos) = pack_entry_raw_end(idx, pack_bytes, entry_offset)? else {
        return Ok(None);
    };
    if zlib_start >= end_pos {
        return Ok(None);
    }
    let compressed = &pack_bytes[zlib_start..end_pos];
    let mut zpos = 0usize;
    let mut scratch = ZlibInflateScratch::default();
    let delta = match scratch.decompress_fixed(compressed, &mut zpos, size) {
        Ok(d) => d,
        Err(_) => return Ok(None),
    };
    Ok(Some((base, delta)))
}

/// Like [`parse_in_pack_delta_reuse_candidate`], but returns the zlib payload and header size
/// without inflating (for verbatim reuse in a new pack).
fn parse_in_pack_delta_reuse_zlib(
    idx: &PackIndex,
    pack_bytes: &[u8],
    entry_offset: u64,
    in_pack: &HashSet<ObjectId>,
) -> Result<Option<(ObjectId, u64, usize, usize)>> {
    let start = entry_offset as usize;
    let hb = idx.hash_bytes();
    let mut p = start;
    let (packed_type, size) = parse_pack_object_header(pack_bytes, &mut p)?;
    let base = match packed_type {
        PackedType::RefDelta => {
            if p + hb > pack_bytes.len() {
                return Err(Error::CorruptObject(
                    "truncated ref-delta base oid while scanning for reuse".to_owned(),
                ));
            }
            let bo = ObjectId::from_bytes(&pack_bytes[p..p + hb])?;
            p += hb;
            bo
        }
        PackedType::OfsDelta => {
            let base_off = parse_ofs_delta_base(pack_bytes, &mut p, entry_offset)?;
            let Some(pos) = idx.find_position_by_offset_sorted(base_off)? else {
                return Ok(None);
            };
            let base_oid_bytes = idx.oid_at(pos);
            if base_oid_bytes.len() != hb {
                return Ok(None);
            }
            ObjectId::from_bytes(base_oid_bytes)?
        }
        _ => return Ok(None),
    };
    if !in_pack.contains(&base) {
        return Ok(None);
    }
    let zlib_start = p;
    let Some(end_pos) = pack_entry_raw_end(idx, pack_bytes, entry_offset)? else {
        return Ok(None);
    };
    if zlib_start >= end_pos {
        return Ok(None);
    }
    let entry_bytes = &pack_bytes[start..end_pos];
    if let Some(crc) = idx.crc32_for_pack_offset(entry_offset)? {
        if crc32fast::hash(entry_bytes) != crc {
            return Ok(None);
        }
    }
    Ok(Some((base, size, zlib_start, end_pos)))
}

fn reuse_delta_matches_pack_resolution(
    idx: &PackIndex,
    target: &ObjectId,
    base: ObjectId,
    delta: &[u8],
) -> bool {
    let Ok(base_obj) = read_object_from_pack(idx, &base) else {
        return false;
    };
    let Ok(target_obj) = read_object_from_pack(idx, target) else {
        return false;
    };
    crate::unpack_objects::apply_delta(&base_obj.data, delta)
        .ok()
        .is_some_and(|applied| applied == target_obj.data)
}

fn on_disk_delta_chain_in_send_map(
    oid: ObjectId,
    immediate_base: &HashMap<ObjectId, ObjectId>,
    in_pack: &HashSet<ObjectId>,
    memo: &mut HashMap<ObjectId, bool>,
) -> bool {
    if let Some(&ok) = memo.get(&oid) {
        return ok;
    }
    let mut cur = oid;
    let mut seen = HashSet::new();
    loop {
        if !seen.insert(cur) {
            memo.insert(oid, true);
            return true;
        }
        if !in_pack.contains(&cur) {
            memo.insert(oid, false);
            return false;
        }
        let Some(&base) = immediate_base.get(&cur) else {
            memo.insert(oid, true);
            return true;
        };
        cur = base;
    }
}

/// Where an object to be packed currently lives among the local packs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PackedSlot {
    /// Position of the pack in oldest-first order (only compared for equality).
    pub pack: u32,
    /// The object is stored whole (not as `OFS_DELTA` / `REF_DELTA`) in that pack.
    pub whole: bool,
}

/// Locate each object of `wanted` in the local packs, oldest pack first, as
/// [`collect_packed_delta_reuse_for_send`] does when choosing which copy to reuse.
///
/// Objects that are not packed are absent from the result.
///
/// # Errors
///
/// Returns [`Error::CorruptObject`] when a pack entry header is malformed, or
/// [`Error::Io`] when a pack cannot be read.
pub(crate) fn locate_packed_objects(
    objects_dir: &Path,
    wanted: &crate::objects::OidHashSet,
) -> Result<crate::objects::OidHashMap<PackedSlot>> {
    let mut indexes = read_local_pack_indexes_cached(objects_dir)?;
    sort_pack_indexes_oldest_first(&mut indexes);
    let mut slots = crate::objects::oid_hash_map_with_capacity(wanted.len());
    for (pack, idx) in (0u32..).zip(&indexes) {
        let bytes = read_pack_bytes_cached(&idx.pack_path)?;
        for entry in idx.iter() {
            let Ok(oid) = ObjectId::from_bytes(entry.oid()) else {
                continue;
            };
            if !wanted.contains(&oid) || slots.contains_key(&oid) {
                continue;
            }
            let mut pos = usize::try_from(entry.offset()?)
                .map_err(|_| Error::CorruptObject("pack offset exceeds memory".to_owned()))?;
            let (packed_type, _) = parse_pack_object_header(&bytes, &mut pos)?;
            let whole = !matches!(packed_type, PackedType::OfsDelta | PackedType::RefDelta);
            slots.insert(oid, PackedSlot { pack, whole });
        }
    }
    Ok(slots)
}

/// Scan local packs once and collect on-disk delta reuse candidates for objects in `send`.
///
/// Copies zlib delta bytes from the source pack (CRC-checked when the index provides CRC32).
/// Callers already hold resolved object payloads in `resolved` for the send set.
///
/// # Errors
///
/// Returns [`Error::CorruptObject`] when a pack stream is malformed.
pub(crate) type OnDiskReuseZlibEntry = (ObjectId, u64, Arc<PackData>, usize, usize);

/// `collect_packed_delta_reuse_for_send` function.
pub fn collect_packed_delta_reuse_for_send(
    objects_dir: &Path,
    send: &HashSet<ObjectId>,
    in_pack: &HashSet<ObjectId>,
    resolved: &HashMap<ObjectId, Object>,
) -> Result<HashMap<ObjectId, OnDiskReuseZlibEntry>> {
    let _ = resolved;
    let mut indexes = read_local_pack_indexes_cached(objects_dir)?;
    sort_pack_indexes_oldest_first(&mut indexes);

    let pack_bytes_by_index: Vec<(Arc<PackIndex>, Arc<PackData>)> = indexes
        .iter()
        .filter(|idx| idx.hash_bytes() == 20)
        .map(|idx| read_pack_bytes_cached(&idx.pack_path).map(|bytes| (Arc::clone(idx), bytes)))
        .collect::<Result<Vec<_>>>()?;

    struct OnDiskDelta {
        base: ObjectId,
        size: u64,
        pack_bytes: Arc<PackData>,
        zlib_start: usize,
        zlib_end: usize,
    }

    let mut on_disk: HashMap<ObjectId, OnDiskDelta> = HashMap::new();
    for (idx, pack_bytes) in &pack_bytes_by_index {
        for entry in idx.iter() {
            let Ok(oid) = ObjectId::from_bytes(entry.oid()) else {
                continue;
            };
            if !send.contains(&oid) || on_disk.contains_key(&oid) {
                continue;
            }
            let Some((base, size, zlib_start, zlib_end)) =
                parse_in_pack_delta_reuse_zlib(idx, pack_bytes, entry.offset()?, in_pack)?
            else {
                continue;
            };
            on_disk.insert(
                oid,
                OnDiskDelta {
                    base,
                    size,
                    pack_bytes: Arc::clone(pack_bytes),
                    zlib_start,
                    zlib_end,
                },
            );
        }
    }

    let immediate_base: HashMap<ObjectId, ObjectId> =
        on_disk.iter().map(|(&oid, d)| (oid, d.base)).collect();

    let mut chain_ok: HashMap<ObjectId, bool> = HashMap::new();
    for &oid in on_disk.keys() {
        on_disk_delta_chain_in_send_map(oid, &immediate_base, in_pack, &mut chain_ok);
    }

    let mut results = HashMap::new();
    for (oid, slot) in on_disk {
        if !chain_ok.get(&oid).copied().unwrap_or(false) {
            continue;
        }
        results.insert(
            oid,
            (
                slot.base,
                slot.size,
                slot.pack_bytes,
                slot.zlib_start,
                slot.zlib_end,
            ),
        );
    }
    Ok(results)
}

/// If `oid` is stored as `REF_DELTA` or `OFS_DELTA` in a local pack and its base OID is in
/// `packed_set`, return the base OID and the **uncompressed** delta payload (Git binary delta).
///
/// Callers re-zlib when writing a new pack so we do not depend on copying raw deflate streams.
///
/// # Errors
///
/// Returns [`Error::CorruptObject`] when the pack stream is malformed.
pub fn packed_ref_delta_reuse_slice(
    objects_dir: &Path,
    oid: &ObjectId,
    packed_set: &HashSet<ObjectId>,
) -> Result<Option<(ObjectId, Vec<u8>)>> {
    let mut indexes = read_local_pack_indexes_cached(objects_dir)?;
    sort_pack_indexes_oldest_first(&mut indexes);
    for idx in indexes {
        if idx.hash_bytes() != 20 {
            continue;
        }
        let Some(entry_offset) = idx.find_offset(oid)? else {
            continue;
        };
        let pack_bytes = read_pack_bytes_cached(&idx.pack_path)?;
        let Some((base, delta)) =
            parse_in_pack_delta_reuse_candidate(&idx, &pack_bytes, entry_offset, packed_set)?
        else {
            continue;
        };
        if reuse_delta_matches_pack_resolution(&idx, oid, base, &delta) {
            return Ok(Some((base, delta)));
        }
    }
    Ok(None)
}

/// Prefer older packs when the same OID exists as a full object in a fresh repack and as a delta
/// in an earlier thin pack (t5316).
fn sort_pack_indexes_oldest_first(indexes: &mut [Arc<PackIndex>]) {
    indexes.sort_by(|a, b| {
        let ta = fs::metadata(&a.pack_path)
            .and_then(|m| m.modified())
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        let tb = fs::metadata(&b.pack_path)
            .and_then(|m| m.modified())
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        ta.cmp(&tb).then_with(|| a.pack_path.cmp(&b.pack_path))
    });
}

fn sort_pack_indexes_newest_first(indexes: &mut [Arc<PackIndex>]) {
    indexes.sort_by(|a, b| {
        let ta = fs::metadata(&a.pack_path)
            .and_then(|m| m.modified())
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        let tb = fs::metadata(&b.pack_path)
            .and_then(|m| m.modified())
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        tb.cmp(&ta).then_with(|| b.pack_path.cmp(&a.pack_path))
    });
}

/// `packed_delta_base_oid` function.
pub fn packed_delta_base_oid(objects_dir: &Path, oid: &ObjectId) -> Result<Option<ObjectId>> {
    let mut indexes = read_local_pack_indexes_cached(objects_dir)?;
    sort_pack_indexes_newest_first(&mut indexes);
    for idx in &indexes {
        if idx.hash_bytes() != 20 {
            continue;
        }
        let Some(entry_offset) = idx.find_offset(oid)? else {
            continue;
        };
        let pack_bytes = read_pack_bytes_cached(&idx.pack_path)?;
        let mut p = entry_offset as usize;
        let (packed_type, _) = parse_pack_object_header(&pack_bytes, &mut p)?;
        match packed_type {
            PackedType::RefDelta => {
                let hb = idx.hash_bytes();
                if p + hb > pack_bytes.len() {
                    return Err(Error::CorruptObject("truncated ref-delta base".to_owned()));
                }
                return Ok(Some(ObjectId::from_bytes(&pack_bytes[p..p + hb])?));
            }
            PackedType::OfsDelta => {
                let base_off = parse_ofs_delta_base(&pack_bytes, &mut p, entry_offset)?;
                return Ok(idx
                    .find_position_by_offset_sorted(base_off)?
                    .and_then(|pos| ObjectId::from_bytes(idx.oid_at(pos)).ok()));
            }
            _ => continue,
        }
    }
    Ok(None)
}

/// End offset (exclusive) of the raw packed bytes for the object at `entry_offset`.
///
/// Uses the next larger pack offset from the index, or the pack trailer when this is the last object.
pub(crate) fn pack_entry_raw_end(
    idx: &PackIndex,
    pack_bytes: &[u8],
    entry_offset: u64,
) -> Result<Option<usize>> {
    let hb = idx.hash_bytes();
    let Some(pack_end) = pack_bytes.len().checked_sub(hb) else {
        return Ok(None);
    };
    let start = entry_offset as usize;
    if start >= pack_end {
        return Ok(None);
    }
    Ok(Some(
        idx.next_pack_offset_after(entry_offset, pack_end as u64)? as usize,
    ))
}

/// When `oid` is stored as a full (non-delta) object in a local pack, return its verbatim packed
/// bytes: the varint type+size header followed by the zlib stream.
///
/// The header of a non-delta pack entry is position-independent, so the returned slice can be
/// copied into a new pack unchanged — skipping both inflate and deflate. This is Git
/// pack-objects' object reuse for objects that stay full in the output pack.
///
/// # Parameters
/// - `objects_dir` — the repository's `objects/` directory.
/// - `oid` — the object to look up.
///
/// Returns `None` when the object is loose only, stored as a delta, or the repository uses a
/// hash width other than SHA-1.
///
/// # Errors
///
/// Returns [`Error::Io`] when pack files cannot be read; a malformed candidate entry is skipped
/// rather than reported so another pack (or the recompression path) can serve the object.
pub fn packed_full_object_slice(objects_dir: &Path, oid: &ObjectId) -> Result<Option<Vec<u8>>> {
    let mut indexes = read_local_pack_indexes_cached(objects_dir)?;
    sort_pack_indexes_newest_first(&mut indexes);
    for idx in &indexes {
        let Some(entry_offset) = idx.find_offset(oid)? else {
            continue;
        };
        let pack_bytes = read_pack_bytes_cached(&idx.pack_path)?;
        let start = entry_offset as usize;
        let mut p = start;
        let Ok((packed_type, _size)) = parse_pack_object_header(&pack_bytes, &mut p) else {
            continue;
        };
        if matches!(packed_type, PackedType::OfsDelta | PackedType::RefDelta) {
            // The same OID may be a full object in another pack; keep scanning.
            continue;
        }
        let Some(end) = pack_entry_raw_end(idx, &pack_bytes, entry_offset)? else {
            continue;
        };
        if end <= start {
            continue;
        }
        let slice = &pack_bytes[start..end];
        // Git `check_pack_crc`: verbatim reuse copies bytes unparsed, so guard with the pack
        // index's CRC32. A corrupt copy is skipped, letting a redundant pack or the normal
        // (validating) read path serve the object instead (t5303).
        let recorded_crc = idx.crc32_for_pack_offset(entry_offset)?;
        match recorded_crc {
            Some(crc) if crc32fast::hash(slice) != crc => continue,
            // v1 indexes carry no CRC; verify by inflating and re-hashing the content.
            None if read_object_from_pack(idx, oid)
                .map(|obj| {
                    HashAlgo::from_len(idx.hash_bytes())
                        .map(|algo| {
                            hash_object(algo, obj.kind, &obj.data)
                                .map(|h| h != *oid)
                                .unwrap_or(true)
                        })
                        .unwrap_or(true)
                })
                .unwrap_or(true) =>
            {
                continue;
            }
            _ => {}
        }
        return Ok(Some(slice.to_vec()));
    }
    Ok(None)
}

pub(crate) fn parse_pack_object_header(bytes: &[u8], pos: &mut usize) -> Result<(PackedType, u64)> {
    let first = *bytes.get(*pos).ok_or_else(|| {
        Error::CorruptObject("unexpected end of pack header while decoding object".to_owned())
    })?;
    *pos += 1;

    let type_code = (first >> 4) & 0x7;
    let mut size = (first & 0x0f) as u64;
    let mut shift = 4u32;
    let mut c = first;
    while (c & 0x80) != 0 {
        c = *bytes.get(*pos).ok_or_else(|| {
            Error::CorruptObject("unexpected end of variable size header".to_owned())
        })?;
        *pos += 1;
        size |= ((c & 0x7f) as u64) << shift;
        shift += 7;
    }

    let packed_type = match type_code {
        1 => PackedType::Commit,
        2 => PackedType::Tree,
        3 => PackedType::Blob,
        4 => PackedType::Tag,
        6 => PackedType::OfsDelta,
        7 => PackedType::RefDelta,
        _ => {
            return Err(Error::CorruptObject(format!(
                "unsupported packed object type {}",
                type_code
            )))
        }
    };
    Ok((packed_type, size))
}

/// Dependency of a packed delta object at `object_offset` within `pack_bytes`.
#[derive(Debug, Clone, Copy)]
pub enum PackedDeltaDependency {
    /// OFS_DELTA: base object offset within the same pack.
    OfsBase {
        /// Pack offset of the base object.
        base_offset: u64,
    },
    /// REF_DELTA: base object id (may live in another pack).
    RefBase {
        /// OID of the delta base.
        base_oid: ObjectId,
    },
}

/// If the object at `object_offset` is a delta, return how it refers to its base.
///
/// `hash_bytes` is the ref-delta base OID width in this pack (`20` for SHA-1, `32` for SHA-256).
pub fn read_packed_delta_dependency(
    pack_bytes: &[u8],
    object_offset: u64,
    hash_bytes: usize,
) -> Result<Option<PackedDeltaDependency>> {
    let mut pos = object_offset as usize;
    let (ty, _) = parse_pack_object_header(pack_bytes, &mut pos)?;
    match ty {
        PackedType::OfsDelta => {
            let base = parse_ofs_delta_base(pack_bytes, &mut pos, object_offset)?;
            Ok(Some(PackedDeltaDependency::OfsBase { base_offset: base }))
        }
        PackedType::RefDelta => {
            if pos + hash_bytes > pack_bytes.len() {
                return Err(Error::CorruptObject("truncated ref-delta base oid".into()));
            }
            let base_oid = ObjectId::from_bytes(&pack_bytes[pos..pos + hash_bytes])?;
            Ok(Some(PackedDeltaDependency::RefBase { base_oid }))
        }
        _ => Ok(None),
    }
}

/// Byte offset where the zlib payload begins for a pack object at `object_offset`.
pub(crate) fn pack_entry_zlib_start(
    pack_bytes: &[u8],
    object_offset: u64,
    hash_bytes: usize,
) -> Result<usize> {
    let mut pos = object_offset as usize;
    let (ty, _) = parse_pack_object_header(pack_bytes, &mut pos)?;
    match ty {
        PackedType::OfsDelta => {
            parse_ofs_delta_base(pack_bytes, &mut pos, object_offset)?;
            Ok(pos)
        }
        PackedType::RefDelta => {
            if pos + hash_bytes > pack_bytes.len() {
                return Err(Error::CorruptObject("truncated ref-delta base oid".into()));
            }
            Ok(pos + hash_bytes)
        }
        _ => Ok(pos),
    }
}

pub(crate) fn parse_ofs_delta_base(bytes: &[u8], pos: &mut usize, this_offset: u64) -> Result<u64> {
    let mut c = *bytes
        .get(*pos)
        .ok_or_else(|| Error::CorruptObject("truncated ofs-delta header".to_owned()))?;
    *pos += 1;
    let mut value = (c & 0x7f) as u64;
    while (c & 0x80) != 0 {
        c = *bytes
            .get(*pos)
            .ok_or_else(|| Error::CorruptObject("truncated ofs-delta header".to_owned()))?;
        *pos += 1;
        value = ((value + 1) << 7) | (c & 0x7f) as u64;
    }
    this_offset
        .checked_sub(value)
        .ok_or_else(|| Error::CorruptObject("invalid ofs-delta base offset".to_owned()))
}

/// Advance `pos` past one packed object (including zlib payload).
///
/// `object_start_offset` is the byte offset of this object within the pack file
/// (used for `OFS_DELTA` base resolution).
/// Raw bytes of one packed object (header + zlib payload) starting at `object_start_offset`.
///
/// `hash_bytes` is the ref-delta base OID width in this pack (`20` for SHA-1, `32` for SHA-256).
pub fn slice_one_pack_object(
    bytes: &[u8],
    object_start_offset: u64,
    hash_bytes: usize,
) -> Result<&[u8]> {
    let start = object_start_offset as usize;
    let mut pos = start;
    skip_one_pack_object(bytes, &mut pos, object_start_offset, hash_bytes)?;
    Ok(&bytes[start..pos])
}

/// `skip_one_pack_object` function.
pub fn skip_one_pack_object(
    bytes: &[u8],
    pos: &mut usize,
    object_start_offset: u64,
    hash_bytes: usize,
) -> Result<()> {
    let (packed_type, size) = parse_pack_object_header(bytes, pos)?;
    let mut scratch = ZlibInflateScratch::default();
    match packed_type {
        PackedType::Commit | PackedType::Tree | PackedType::Blob | PackedType::Tag => {
            scratch.skip_zlib_stream(bytes, pos, size)?;
        }
        PackedType::RefDelta => {
            if *pos + hash_bytes > bytes.len() {
                return Err(Error::CorruptObject("truncated ref-delta base oid".into()));
            }
            *pos += hash_bytes;
            scratch.skip_zlib_stream(bytes, pos, size)?;
        }
        PackedType::OfsDelta => {
            let _base_off = parse_ofs_delta_base(bytes, pos, object_start_offset)?;
            scratch.skip_zlib_stream(bytes, pos, size)?;
        }
    }
    Ok(())
}

/// Read all object IDs from a `.idx` file.
pub fn read_idx_object_ids(idx_path: &Path) -> Result<Vec<ObjectId>> {
    let index = read_pack_index(idx_path)?;
    let mut out = Vec::new();
    let hash_bytes = index.hash_bytes();
    for e in index.iter() {
        if e.oid().len() == hash_bytes {
            out.push(ObjectId::from_bytes(e.oid())?);
        }
    }
    Ok(out)
}

mod pack_cache_test_sync {
    use std::cell::Cell;
    use std::sync::Mutex;
    use std::sync::MutexGuard;

    /// Process-global pack cache test lock state (guards real synchronization state).
    struct PackCacheTestCoordinator {
        top_level_guards: u32,
    }

    // hygiene: pack cache test synchronization (cfg(test) only)
    static COORD: Mutex<PackCacheTestCoordinator> = Mutex::new(PackCacheTestCoordinator {
        top_level_guards: 0,
    });

    // hygiene: pack cache test depth guard (cfg(test) only)
    thread_local! {
        static DEPTH: Cell<u32> = const { Cell::new(0) };
    }

    /// Serializes tests that share the process-global pack cache (including `clear_pack_cache`).
    pub struct PackCacheTestGuard {
        _inner: Option<MutexGuard<'static, PackCacheTestCoordinator>>,
    }

    pub fn acquire() -> PackCacheTestGuard {
        let depth = DEPTH.with(|d| {
            let n = d.get();
            d.set(n + 1);
            n
        });
        PackCacheTestGuard {
            _inner: if depth == 0 {
                let mut g = COORD.lock().unwrap_or_else(|e| e.into_inner());
                assert_eq!(
                    g.top_level_guards, 0,
                    "pack cache test coordinator already held"
                );
                g.top_level_guards = 1;
                Some(g)
            } else {
                None
            },
        }
    }

    impl Drop for PackCacheTestGuard {
        fn drop(&mut self) {
            DEPTH.with(|d| {
                let n = d.get();
                assert!(n > 0, "pack cache test guard depth underflow");
                d.set(n - 1);
                if n == 1 {
                    if let Some(mut g) = self._inner.take() {
                        assert_eq!(g.top_level_guards, 1);
                        g.top_level_guards = 0;
                    }
                }
            });
        }
    }
}

#[cfg(test)]
pub(crate) fn pack_cache_test_guard() -> pack_cache_test_sync::PackCacheTestGuard {
    pack_cache_test_sync::acquire()
}

/// Serializes tests that mutate the process-global pack cache (integration tests).
pub fn test_pack_cache_guard() -> pack_cache_test_sync::PackCacheTestGuard {
    pack_cache_test_sync::acquire()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::delta_encode::encode_lcp_delta;
    use crate::odb::Odb;
    use crate::pack_map::PackData;
    use crate::transfer::{build_pack, PackBuildOptions};
    use flate2::write::ZlibEncoder;
    use flate2::Compression;
    use std::io::Read;
    use std::io::Write;
    use std::process::Command;

    fn git_try(dir: &std::path::Path, args: &[&str]) -> bool {
        Command::new("git") // hygiene: git fixture comparison in cfg(test) module // hygiene: git fixture comparison in cfg(test) module
            .current_dir(dir)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    fn git(dir: &std::path::Path, args: &[&str]) {
        let out = Command::new("git") // hygiene: git fixture comparison in cfg(test) module
            .current_dir(dir)
            .args(args)
            .env("GIT_AUTHOR_NAME", "T")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "T")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .env("GIT_AUTHOR_DATE", "2005-04-07T22:13:13 +0200")
            .env("GIT_COMMITTER_DATE", "2005-04-07T22:13:13 +0200")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .expect("git");
        assert!(
            out.status.success(),
            "git {:?}: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn append_pack_object_header(buf: &mut Vec<u8>, type_code: u8, payload_len: usize) {
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

    fn zlib_pack(payload: &[u8]) -> Vec<u8> {
        let mut enc = ZlibEncoder::new(Vec::new(), Compression::default());
        enc.write_all(payload).unwrap();
        enc.finish().unwrap()
    }

    fn append_sha1_pack_trailer(buf: &mut Vec<u8>) {
        buf.extend_from_slice(HashAlgo::Sha1.checksum_digest(&*buf).as_slice());
    }

    fn append_ref_delta(buf: &mut Vec<u8>, base_oid: &ObjectId, delta: &[u8]) {
        let compressed = zlib_pack(delta);
        append_pack_object_header(buf, 7, delta.len());
        buf.extend_from_slice(base_oid.as_bytes());
        buf.extend_from_slice(&compressed);
    }

    fn append_ofs_delta(buf: &mut Vec<u8>, base_offset: u64, delta: &[u8]) -> u64 {
        let object_start = buf.len() as u64;
        let compressed = zlib_pack(delta);
        append_pack_object_header(buf, 6, delta.len());
        let mut ofs = object_start - base_offset;
        while ofs >= 0x80 {
            buf.push((ofs as u8 & 0x7f) | 0x80);
            ofs >>= 7;
        }
        buf.push(ofs as u8);
        buf.extend_from_slice(&compressed);
        object_start
    }

    fn append_whole_blob(buf: &mut Vec<u8>, data: &[u8]) -> u64 {
        let off = buf.len() as u64;
        let compressed = zlib_pack(data);
        append_pack_object_header(buf, 3, data.len());
        buf.extend_from_slice(&compressed);
        off
    }

    fn append_corrupt_whole_blob(buf: &mut Vec<u8>) -> u64 {
        let off = buf.len() as u64;
        append_pack_object_header(buf, 3, 8);
        buf.extend_from_slice(&[0xff, 0xfe, 0xfd]);
        off
    }

    fn write_v2_idx_for_test(
        idx_path: &Path,
        pack_path: &Path,
        entries: &[(ObjectId, u64)],
    ) -> Result<()> {
        let pack_bytes = std::fs::read(pack_path)?;
        let mut with_crc = Vec::with_capacity(entries.len());
        for (oid, off) in entries {
            let start = *off as usize;
            let mut end = start;
            skip_one_pack_object(&pack_bytes, &mut end, *off, 20)
                .map_err(|e| Error::CorruptObject(format!("idx crc walk: {e}")))?;
            with_crc.push((*oid, *off, crc32fast::hash(&pack_bytes[start..end])));
        }
        write_v2_pack_index(idx_path, pack_path, &with_crc, 20)
    }

    fn single_blob_pack(data: &[u8]) -> (Vec<u8>, ObjectId, u64) {
        let odb = Odb::new(tempfile::tempdir().expect("tempdir").path());
        let oid = odb.hash(ObjectKind::Blob, data).expect("hash");
        let mut pack = Vec::new();
        pack.extend_from_slice(b"PACK");
        pack.extend_from_slice(&2u32.to_be_bytes());
        pack.extend_from_slice(&1u32.to_be_bytes());
        let off = append_whole_blob(&mut pack, data);
        append_sha1_pack_trailer(&mut pack);
        (pack, oid, off)
    }

    #[test]
    fn single_local_pack_reader_batch_check_caches_deltified_entries() {
        let _guard = pack_cache_test_guard();
        clear_pack_cache();
        let Some((tmp, idx, tip, odb)) = build_ofs_delta_chain_pack(2) else {
            return;
        };
        let objects = tmp.path().join("objects");
        let store = crate::pack_store::PackStore::legacy_for_objects_dir(&objects);
        crate::pack_store::PackStore::with_context(store, || {
            let mut reader = SingleLocalPackReader::open(&objects)
                .expect("open")
                .expect("single pack");
            let info1 = reader.read_info(&tip).expect("first read");
            let info2 = reader.read_info(&tip).expect("cached read");
            assert_eq!(info1, info2);
            let full = read_object_from_pack(&idx, &tip).expect("full read");
            assert_eq!(info1.kind, full.kind);
            assert_eq!(info1.size, full.data.len() as u64);
        });
        let _ = odb;
    }

    #[test]
    fn pack_read_info_cache_is_scoped_per_repository_and_pack_bytes() {
        let _guard = pack_cache_test_guard();
        clear_pack_cache();
        const ENTRY_OFFSET: u64 = 200;
        let dir_a = tempfile::tempdir().expect("tempdir a");
        let dir_b = tempfile::tempdir().expect("tempdir b");
        let objects_a = dir_a.path().join("objects");
        let objects_b = dir_b.path().join("objects");
        fs::create_dir_all(objects_a.join("pack")).expect("pack dir a");
        fs::create_dir_all(objects_b.join("pack")).expect("pack dir b");
        let pack_a = objects_a.join("pack/p-a.pack");
        let pack_b = objects_b.join("pack/p-b.pack");
        let (oid_a, _) =
            write_sparse_single_blob_pack(dir_a.path(), &pack_a, b"five!", ENTRY_OFFSET)
                .expect("pack a");
        let (oid_b, _) =
            write_sparse_single_blob_pack(dir_b.path(), &pack_b, b"1234567890", ENTRY_OFFSET)
                .expect("pack b");
        let idx_a = pack_a.with_extension("idx");
        let idx_b = pack_b.with_extension("idx");
        write_v2_idx_for_test(&idx_a, &pack_a, &[(oid_a, ENTRY_OFFSET)]).expect("idx a");
        write_v2_idx_for_test(&idx_b, &pack_b, &[(oid_b, ENTRY_OFFSET)]).expect("idx b");
        let idx_a = read_pack_index(&idx_a).expect("parse idx a");
        let idx_b = read_pack_index(&idx_b).expect("parse idx b");

        let read_size = |objects: &Path, idx: &PackIndex, oid: &ObjectId| -> u64 {
            let store = crate::pack_store::PackStore::legacy_for_objects_dir(objects);
            crate::pack_store::PackStore::with_context(store, || {
                super::read_object_info_from_pack_at_offset(idx, ENTRY_OFFSET)
                    .expect("read_info")
                    .size
            })
        };

        let size_a = read_size(&objects_a, &idx_a, &oid_a);
        let size_b = read_size(&objects_b, &idx_b, &oid_b);
        assert_eq!(size_a, 5);
        assert_eq!(size_b, 10);
        // A → B → A again: cache must not return B's size for A's pack entry.
        assert_eq!(read_size(&objects_a, &idx_a, &oid_a), 5);
        assert_eq!(read_size(&objects_b, &idx_b, &oid_b), 10);
        assert_eq!(read_size(&objects_a, &idx_a, &oid_a), 5);
    }
    /// Write a valid pack with one blob placed at `object_offset` so the on-disk file
    /// length exceeds the mmap owned-buffer threshold without storing huge compressed payloads.
    fn write_sparse_single_blob_pack(
        repo_root: &Path,
        pack_path: &Path,
        data: &[u8],
        object_offset: u64,
    ) -> Result<(ObjectId, u64)> {
        use std::fs::OpenOptions;
        use std::io::{Seek, SeekFrom, Write};

        let odb = Odb::new(repo_root);
        let oid = odb.hash(ObjectKind::Blob, data).expect("hash");
        let mut file = OpenOptions::new()
            .create(true)
            .write(true)
            .read(true)
            .truncate(true)
            .open(pack_path)
            .map_err(Error::Io)?;
        file.write_all(b"PACK").map_err(Error::Io)?;
        file.write_all(&2u32.to_be_bytes()).map_err(Error::Io)?;
        file.write_all(&1u32.to_be_bytes()).map_err(Error::Io)?;
        file.seek(SeekFrom::Start(object_offset))
            .map_err(Error::Io)?;
        let mut tail = Vec::new();
        append_whole_blob(&mut tail, data);
        file.write_all(&tail).map_err(Error::Io)?;
        let trailer = sha1_trailer_for_open_file(&mut file)?;
        file.write_all(trailer.as_bytes()).map_err(Error::Io)?;
        file.flush().map_err(Error::Io)?;
        Ok((oid, object_offset))
    }

    #[test]
    fn skip_one_pack_object_rejects_zero_size_with_zlib_payload() {
        let mut bytes = Vec::new();
        bytes.push(0x30);
        bytes.extend_from_slice(&zlib_pack(b"x"));
        let mut pos = 0usize;
        let err = skip_one_pack_object(&bytes, &mut pos, 0, 20).unwrap_err();
        assert!(matches!(err, Error::CorruptObject(_)), "got {err:?}");
    }

    fn install_synthetic_pack(
        objects_dir: &Path,
        stem: &str,
        pack: &[u8],
        entries: &[(ObjectId, u64)],
    ) -> PackIndex {
        install_synthetic_pack_inner(objects_dir, stem, pack, entries, true)
    }

    fn install_synthetic_pack_no_clear(
        objects_dir: &Path,
        stem: &str,
        pack: &[u8],
        entries: &[(ObjectId, u64)],
    ) -> PackIndex {
        install_synthetic_pack_inner(objects_dir, stem, pack, entries, false)
    }

    fn install_synthetic_pack_inner(
        objects_dir: &Path,
        stem: &str,
        pack: &[u8],
        entries: &[(ObjectId, u64)],
        clear_cache: bool,
    ) -> PackIndex {
        let pack_dir = objects_dir.join("pack");
        std::fs::create_dir_all(&pack_dir).expect("pack dir");
        let pack_path = pack_dir.join(format!("{stem}.pack"));
        let idx_path = pack_dir.join(format!("{stem}.idx"));
        std::fs::write(&pack_path, pack).expect("write pack");
        write_v2_idx_for_test(&idx_path, &pack_path, entries).expect("write idx");
        if clear_cache {
            clear_pack_cache();
        }
        read_pack_index(&idx_path).expect("read idx")
    }

    fn pack_index_for_objects(
        pack_path: std::path::PathBuf,
        objects: &[(ObjectId, u64)],
    ) -> PackIndex {
        let idx_path = pack_path.with_extension("idx");
        let pack_bytes = fs::read(&pack_path).expect("read pack for idx trailer");
        let trailer = &pack_bytes[pack_bytes.len().saturating_sub(20)..];
        let rows: Vec<(ObjectId, u64, u32)> = objects
            .iter()
            .map(|(oid, offset)| (*oid, *offset, 0))
            .collect();
        write_v2_pack_index_with_trailer(&idx_path, &rows, trailer, 20, None).expect("write idx");
        read_pack_index(&idx_path).expect("read idx")
    }

    fn index_pack_in_scratch(pack: &[u8]) -> (tempfile::TempDir, PackIndex) {
        let dir = tempfile::tempdir().expect("tempdir");
        let pack_path = dir.path().join("chain.pack");
        std::fs::write(&pack_path, pack).expect("write pack");
        let out = Command::new("git") // hygiene: git fixture comparison in cfg(test) module
            .current_dir(dir.path())
            .args(["index-pack", "chain.pack"])
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .expect("index-pack");
        assert!(
            out.status.success(),
            "git index-pack failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let idx_path = dir.path().join("chain.idx");
        let idx = read_pack_index(&idx_path).expect("read idx");
        (dir, idx)
    }

    /// Repo with many prefix-preserving edits to one blob so delta chains can exceed 50.
    fn build_deep_delta_repo(commit_count: usize) -> Option<(tempfile::TempDir, ObjectId, Odb)> {
        let tmp = tempfile::tempdir().ok()?;
        let dir = tmp.path();
        if !git_try(dir, &["init", "-q", "-b", "main", "."]) {
            return None;
        }
        let mut body = String::new();
        for i in 0..200 {
            body.push_str(&format!("seed line {i:04}\n"));
        }
        for rev in 0..commit_count {
            body.push_str(&format!("edit-{rev}\n"));
            std::fs::write(dir.join("blob.txt"), body.as_bytes()).ok()?;
            git(dir, &["add", "blob.txt"]);
            git(dir, &["commit", "-q", "-m", &format!("c{rev}")]);
        }
        let tip_hex = Command::new("git") // hygiene: git fixture comparison in cfg(test) module
            .current_dir(dir)
            .args(["rev-parse", "HEAD"])
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .ok()?;
        if !tip_hex.status.success() {
            return None;
        }
        let tip = ObjectId::from_hex(std::str::from_utf8(&tip_hex.stdout).ok()?.trim()).ok()?;
        let git_dir = dir.join(".git");
        let odb = Odb::new(&git_dir.join("objects")).with_config_git_dir(git_dir);
        Some((tmp, tip, odb))
    }

    #[test]
    fn verify_packed_object_hash_sha256_detects_corruption() {
        use crate::hash::hash_object;
        use std::fs;

        let data = b"sha256 packed blob for hash verify\n";
        let oid = hash_object(HashAlgo::Sha256, ObjectKind::Blob, data).expect("hash");
        let wrong_oid =
            hash_object(HashAlgo::Sha256, ObjectKind::Blob, b"different payload\n").expect("hash");
        let mut pack = Vec::new();
        pack.extend_from_slice(b"PACK");
        pack.extend_from_slice(&2u32.to_be_bytes());
        pack.extend_from_slice(&1u32.to_be_bytes());
        let off = append_whole_blob(&mut pack, data);
        pack.extend_from_slice(HashAlgo::Sha256.checksum_digest(&pack).as_slice());

        let dir = tempfile::tempdir().expect("tempdir");
        let pack_path = dir.path().join("sha256-corrupt.pack");
        fs::write(&pack_path, &pack).expect("write pack");

        let idx_path = pack_path.with_extension("idx");
        let trailer = &pack[pack.len() - 32..];
        write_v2_pack_index_with_trailer(&idx_path, &[(wrong_oid, off, 0)], trailer, 32, None)
            .expect("write idx");
        let idx = read_pack_index(&idx_path).expect("read idx");

        let err = read_object_from_pack_bytes(&pack, &idx, wrong_oid.as_bytes()).unwrap_err();
        match err {
            Error::CorruptObject(msg) => {
                assert!(
                    msg.contains("hashes to"),
                    "expected hash mismatch message, got: {msg}"
                );
                assert!(
                    msg.contains(&oid.to_hex()),
                    "expected recomputed oid in message, got: {msg}"
                );
            }
            other => panic!("expected CorruptObject, got {other:?}"),
        }
    }

    #[test]
    fn reads_delta_chain_deeper_than_50() {
        let _guard = pack_cache_test_guard();
        let Some((_repo, tip, odb)) = build_deep_delta_repo(120) else {
            let _ = "SKIP: git unavailable for deep delta fixture";
            return;
        };
        let pack = build_pack(
            &odb,
            &[tip],
            &[],
            &PackBuildOptions {
                delta: true,
                max_depth: 120,
                use_ofs_delta: true,
                ..PackBuildOptions::default()
            },
        )
        .expect("build deep delta pack");

        let (_scratch, idx) = index_pack_in_scratch(&pack);
        assert!(
            idx.len() > 1,
            "expected a multi-object pack, got {}",
            idx.len()
        );

        for entry in idx.iter() {
            let oid = ObjectId::from_bytes(entry.oid()).expect("oid in idx");
            let from_pack = read_object_from_pack(&idx, &oid).expect("read packed object");
            let expected = odb.read(&oid).expect("loose/alt copy for oid");
            assert_eq!(from_pack.kind, expected.kind);
            assert_eq!(from_pack.data, expected.data);
            assert_eq!(
                odb.hash(from_pack.kind, &from_pack.data).expect("hash"),
                oid
            );
        }
    }

    #[test]
    fn cyclic_ref_delta_chain_errors() {
        let _guard = pack_cache_test_guard();
        let odb = Odb::new(tempfile::tempdir().expect("tempdir").path());
        let content_a = b"aaa".as_slice();
        let content_b = b"bbb".as_slice();
        let oid_a = odb.hash(ObjectKind::Blob, content_a).expect("hash");
        let oid_b = odb.hash(ObjectKind::Blob, content_b).expect("hash");
        let delta_a = encode_lcp_delta(content_b, content_a).expect("delta a");
        let delta_b = encode_lcp_delta(content_a, content_b).expect("delta b");

        let mut pack = Vec::new();
        pack.extend_from_slice(b"PACK");
        pack.extend_from_slice(&2u32.to_be_bytes());
        pack.extend_from_slice(&2u32.to_be_bytes());
        let off_a = pack.len() as u64;
        append_ref_delta(&mut pack, &oid_b, &delta_a);
        let off_b = pack.len() as u64;
        append_ref_delta(&mut pack, &oid_a, &delta_b);
        append_sha1_pack_trailer(&mut pack);

        let dir = tempfile::tempdir().expect("tempdir");
        let pack_path = dir.path().join("cycle.pack");
        std::fs::write(&pack_path, &pack).expect("write cyclic pack");
        let idx = pack_index_for_objects(pack_path, &[(oid_a, off_a), (oid_b, off_b)]);
        let pack_bytes = std::fs::read(&idx.pack_path).expect("read pack");
        for err in [
            read_object_from_pack(&idx, &oid_a).expect_err("read_object_from_pack"),
            read_object_from_pack_bytes(&pack_bytes, &idx, oid_a.as_bytes())
                .expect_err("read_object_from_pack_bytes"),
        ] {
            assert!(
                matches!(err, Error::DeltaChainTooDeep { limit: 4096 }),
                "expected typed delta chain limit, got {err:?}"
            );
        }
    }

    #[test]
    fn resolve_cross_pack_delta_without_index_clone() {
        let _guard = pack_cache_test_guard();
        clear_pack_cache();
        let odb = Odb::new(tempfile::tempdir().expect("tempdir").path());
        let base = b"cross-pack-base-payload".as_slice();
        let tip = b"cross-pack-base-payload!".as_slice();
        let oid_base = odb.hash(ObjectKind::Blob, base).expect("hash");
        let oid_tip = odb.hash(ObjectKind::Blob, tip).expect("hash");
        let delta = encode_lcp_delta(base, tip).expect("delta");

        let mut pack_base = Vec::new();
        pack_base.extend_from_slice(b"PACK");
        pack_base.extend_from_slice(&2u32.to_be_bytes());
        pack_base.extend_from_slice(&1u32.to_be_bytes());
        let off_base = append_whole_blob(&mut pack_base, base);
        append_sha1_pack_trailer(&mut pack_base);

        let mut pack_delta = Vec::new();
        pack_delta.extend_from_slice(b"PACK");
        pack_delta.extend_from_slice(&2u32.to_be_bytes());
        pack_delta.extend_from_slice(&1u32.to_be_bytes());
        let off_tip = pack_delta.len() as u64;
        append_ref_delta(&mut pack_delta, &oid_base, &delta);
        append_sha1_pack_trailer(&mut pack_delta);

        let dir = tempfile::tempdir().expect("tempdir");
        let objects = dir.path().join("objects");
        let _idx_base =
            install_synthetic_pack(&objects, "cross-base", &pack_base, &[(oid_base, off_base)]);
        let idx_delta =
            install_synthetic_pack(&objects, "cross-delta", &pack_delta, &[(oid_tip, off_tip)]);

        let from_delta_pack = read_object_from_pack(&idx_delta, &oid_tip).expect("delta pack read");
        assert_eq!(from_delta_pack.kind, ObjectKind::Blob);
        assert_eq!(from_delta_pack.data, tip);
        assert_eq!(
            odb.hash(from_delta_pack.kind, &from_delta_pack.data)
                .expect("hash"),
            oid_tip
        );

        let from_packs = read_object_from_packs(&objects, &oid_tip).expect("packs dir read");
        assert_eq!(from_packs.kind, ObjectKind::Blob);
        assert_eq!(from_packs.data, tip);
        assert_eq!(
            odb.hash(from_packs.kind, &from_packs.data).expect("hash"),
            oid_tip
        );
    }

    #[test]
    fn read_object_from_pack_bytes_uses_in_memory_index_without_on_disk_idx() {
        let odb = Odb::new(tempfile::tempdir().expect("tempdir").path());
        let content = b"in-memory-index-blob";
        let oid = odb.hash(ObjectKind::Blob, content).expect("hash");

        let mut pack = Vec::new();
        pack.extend_from_slice(b"PACK");
        pack.extend_from_slice(&2u32.to_be_bytes());
        pack.extend_from_slice(&1u32.to_be_bytes());
        let off = append_whole_blob(&mut pack, content);
        append_sha1_pack_trailer(&mut pack);

        let dir = tempfile::tempdir().expect("tempdir");
        let pack_path = dir.path().join("mem.pack");
        std::fs::write(&pack_path, &pack).expect("write pack");
        let idx = pack_index_for_objects(pack_path, &[(oid, off)]);
        std::fs::remove_file(&idx.idx_path).expect("drop on-disk idx");
        assert!(
            !idx.idx_path.exists(),
            "fixture must not rely on an on-disk .idx"
        );

        let got = read_object_from_pack_bytes(&pack, &idx, oid.as_bytes()).expect("read bytes");
        assert_eq!(got.kind, ObjectKind::Blob);
        assert_eq!(got.data, content);
    }

    fn build_ofs_delta_chain_pack(
        depth: usize,
    ) -> Option<(tempfile::TempDir, PackIndex, ObjectId, Odb)> {
        if depth == 0 {
            return None;
        }
        let tmp = tempfile::tempdir().ok()?;
        let odb = Odb::new(&tmp.path().join("objects"));
        let mut content = b"ofs-delta-chain-base\n".to_vec();
        let mut pack = Vec::new();
        pack.extend_from_slice(b"PACK");
        pack.extend_from_slice(&2u32.to_be_bytes());
        pack.extend_from_slice(&(depth as u32 + 1).to_be_bytes());
        let mut base_off = append_whole_blob(&mut pack, &content);
        let mut entries = vec![(
            odb.hash(ObjectKind::Blob, &content).expect("hash"),
            base_off,
        )];
        for _ in 0..depth {
            let mut next = content.clone();
            next.push(b'!');
            let delta = encode_lcp_delta(&content, &next).ok()?;
            let off = append_ofs_delta(&mut pack, base_off, &delta);
            let oid = odb.hash(ObjectKind::Blob, &next).expect("hash");
            entries.push((oid, off));
            content = next;
            base_off = off;
        }
        append_sha1_pack_trailer(&mut pack);
        let tip = entries.last()?.0;
        let dir = tmp.path();
        std::fs::create_dir_all(dir.join("objects/pack")).ok()?;
        let pack_path = dir.join("objects/pack/chain.pack");
        std::fs::write(&pack_path, &pack).ok()?;
        write_v2_idx_for_test(&pack_path.with_extension("idx"), &pack_path, &entries).ok()?;
        clear_pack_cache();
        let idx = read_pack_index(&pack_path.with_extension("idx")).ok()?;
        Some((tmp, idx, tip, odb))
    }

    #[test]
    fn ofs_delta_chain_depth_4096_resolves() {
        let _guard = pack_cache_test_guard();
        clear_pack_cache();
        let Some((_tmp, idx, tip, odb)) = build_ofs_delta_chain_pack(4096) else {
            return;
        };
        let got = read_object_from_pack(&idx, &tip).expect("4096-deep chain");
        let expect = odb.hash(ObjectKind::Blob, &got.data).expect("hash");
        assert_eq!(expect, tip);
    }

    #[test]
    fn ofs_delta_chain_depth_4097_errors() {
        let _guard = pack_cache_test_guard();
        clear_pack_cache();
        let Some((_tmp, idx, tip, _odb)) = build_ofs_delta_chain_pack(4097) else {
            return;
        };
        let err = read_object_from_pack(&idx, &tip).expect_err("4097-deep chain");
        assert!(
            matches!(err, Error::DeltaChainTooDeep { limit: 4096 }),
            "got {err:?}"
        );
    }

    #[test]
    fn delta_base_cache_limit_zero_disables_caching() {
        let _guard = pack_cache_test_guard();
        clear_pack_cache();
        let Some((_tmp, idx, tip, _odb)) = build_ofs_delta_chain_pack(12) else {
            return;
        };
        pack_cache::test_set_delta_base_cache_byte_limit(&idx.pack_path, 0);
        let _ = read_object_from_pack(&idx, &tip).expect("read chain");
        assert_eq!(
            pack_cache::test_delta_base_cache_bytes_used(&idx.pack_path),
            0,
            "limit 0 must not retain delta bases"
        );
        pack_cache::test_set_delta_base_cache_byte_limit(
            &idx.pack_path,
            pack_cache::DELTA_BASE_CACHE_DEFAULT,
        );
    }

    #[test]
    fn delta_base_cache_eviction_respects_byte_limit() {
        let _guard = pack_cache_test_guard();
        clear_pack_cache();
        let Some((_tmp, idx, tip, _odb)) = build_ofs_delta_chain_pack(32) else {
            return;
        };
        let expected = read_object_from_pack(&idx, &tip)
            .expect("baseline read")
            .data;
        pack_cache::test_set_delta_base_cache_byte_limit(&idx.pack_path, 4096);
        let _ = read_object_from_pack(&idx, &tip).expect("prime cache");
        assert!(
            pack_cache::test_delta_base_cache_bytes_used(&idx.pack_path) <= 4096,
            "cache bytes {} exceeded cap",
            pack_cache::test_delta_base_cache_bytes_used(&idx.pack_path)
        );
        pack_cache::test_set_delta_base_cache_byte_limit(&idx.pack_path, 512);
        for round in 0..8 {
            let got = read_object_from_pack(&idx, &tip).expect("read after eviction");
            assert_eq!(got.data, expected, "round {round} must stay byte-identical");
        }
        pack_cache::test_set_delta_base_cache_byte_limit(
            &idx.pack_path,
            pack_cache::DELTA_BASE_CACHE_DEFAULT,
        );
    }

    #[test]
    fn delta_base_cache_invalidated_when_pack_bytes_reloaded() {
        use filetime::{set_file_mtime, FileTime};
        let _guard = pack_cache_test_guard();
        clear_pack_cache();
        let Some((_tmp, idx, tip, _odb)) = build_ofs_delta_chain_pack(8) else {
            return;
        };
        let pack_path = idx.pack_path.clone();
        let pack_id = pack_cache::pack_id_for(&pack_path);
        let mid_off = idx
            .iter()
            .nth(4)
            .map(|e| e.offset())
            .expect("mid hop")
            .expect("mid off");
        let _ = read_object_from_pack(&idx, &tip).expect("warm cache");
        assert!(
            pack_cache::test_delta_base_cached(&pack_path, pack_id, mid_off),
            "expected intermediate cached at offset {mid_off}"
        );
        set_file_mtime(&pack_path, FileTime::now()).expect("touch pack mtime");
        assert!(
            pack_cache::revalidate_stale_pack_bytes(&pack_path).expect("revalidate"),
            "expected pack bytes reload"
        );
        assert!(
            !pack_cache::test_delta_base_cached(&pack_path, pack_id, mid_off),
            "cache must drop when pack bytes reload"
        );
    }

    #[test]
    fn git_repack_depth_250_matches_cat_file_batch() {
        let _guard = pack_cache_test_guard();
        clear_pack_cache();
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path();
        if Command::new("git") // hygiene: git fixture comparison in cfg(test) module
            .current_dir(dir)
            .args(["init", "-q"])
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .status()
            .is_err()
        {
            let _ = "SKIP: git unavailable";
            return;
        }
        for i in 0..64 {
            std::fs::write(dir.join(format!("f{i}.txt")), format!("payload {i}\n")).unwrap();
            git(dir, &["add", &format!("f{i}.txt")]);
            git(dir, &["commit", "-qm", &format!("c{i}")]);
        }
        git(dir, &["repack", "-adf", "--depth=250", "--window=250"]);
        let objects = dir.join(".git/objects");
        let indexes = read_local_pack_indexes_cached(&objects).expect("pack indexes");
        let idx = indexes
            .into_iter()
            .max_by_key(|i| i.len())
            .expect("repack pack");
        for entry in idx.iter() {
            let oid = ObjectId::from_bytes(entry.oid()).expect("oid");
            let grit_obj = read_object_from_pack(&idx, &oid).expect("grit read");
            let git_type = Command::new("git") // hygiene: git fixture comparison in cfg(test) module
                .current_dir(dir)
                .args(["cat-file", "-t", &oid.to_hex()])
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .output()
                .expect("git cat-file -t");
            assert!(git_type.status.success());
            let git_kind = ObjectKind::from_bytes(
                std::str::from_utf8(git_type.stdout.trim_ascii_end())
                    .expect("utf8 type")
                    .as_bytes(),
            )
            .expect("git type");
            assert_eq!(git_kind, grit_obj.kind);
            let type_arg = match grit_obj.kind {
                ObjectKind::Blob => "blob",
                ObjectKind::Tree => "tree",
                ObjectKind::Commit => "commit",
                ObjectKind::Tag => "tag",
            };
            let git_payload = Command::new("git") // hygiene: git fixture comparison in cfg(test) module
                .current_dir(dir)
                .args(["cat-file", type_arg, &oid.to_hex()])
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .output()
                .expect("git cat-file raw");
            assert!(git_payload.status.success());
            assert_eq!(git_payload.stdout, grit_obj.data);
        }
    }

    #[test]
    fn cyclic_ref_delta_chain_across_two_packs_errors() {
        let _guard = pack_cache_test_guard();
        clear_pack_cache();
        let odb = Odb::new(tempfile::tempdir().expect("tempdir").path());
        let content_a = b"aaa".as_slice();
        let content_b = b"bbb".as_slice();
        let oid_a = odb.hash(ObjectKind::Blob, content_a).expect("hash");
        let oid_b = odb.hash(ObjectKind::Blob, content_b).expect("hash");
        let delta_a = encode_lcp_delta(content_b, content_a).expect("delta a");
        let delta_b = encode_lcp_delta(content_a, content_b).expect("delta b");

        let mut pack_a = Vec::new();
        pack_a.extend_from_slice(b"PACK");
        pack_a.extend_from_slice(&2u32.to_be_bytes());
        pack_a.extend_from_slice(&1u32.to_be_bytes());
        let off_a = pack_a.len() as u64;
        append_ref_delta(&mut pack_a, &oid_b, &delta_a);
        append_sha1_pack_trailer(&mut pack_a);

        let mut pack_b = Vec::new();
        pack_b.extend_from_slice(b"PACK");
        pack_b.extend_from_slice(&2u32.to_be_bytes());
        pack_b.extend_from_slice(&1u32.to_be_bytes());
        let off_b = pack_b.len() as u64;
        append_ref_delta(&mut pack_b, &oid_a, &delta_b);
        append_sha1_pack_trailer(&mut pack_b);

        let dir = tempfile::tempdir().expect("tempdir");
        let objects = dir.path().join("objects");
        let idx_b = install_synthetic_pack(&objects, "cycle-b", &pack_b, &[(oid_b, off_b)]);
        let idx_a = install_synthetic_pack(&objects, "cycle-a", &pack_a, &[(oid_a, off_a)]);
        assert_eq!(idx_a.len(), 1);
        assert_eq!(idx_b.len(), 1);
        assert!(idx_b.contains(&oid_b).expect("contains"));
        let _ = idx_b;

        let err = read_object_from_pack(&idx_a, &oid_a).expect_err("cross-pack cycle");
        assert!(
            matches!(err, Error::DeltaChainTooDeep { limit: 4096 }),
            "expected typed delta chain limit, got {err:?}"
        );
    }

    #[test]
    fn cross_pack_ref_delta_resolves_through_odb_read() {
        let _guard = pack_cache_test_guard();
        clear_pack_cache();
        let tmp = tempfile::tempdir().expect("tempdir");
        let objects = tmp.path().join("objects");
        let odb = Odb::new(&objects);
        let base = b"common prefix base";
        let target = b"common prefix target";
        let oid_base = odb.hash(ObjectKind::Blob, base).expect("hash");
        let delta = encode_lcp_delta(base, target).expect("delta");
        let oid_target = odb.hash(ObjectKind::Blob, target).expect("hash");

        let mut pack_base = Vec::new();
        pack_base.extend_from_slice(b"PACK");
        pack_base.extend_from_slice(&2u32.to_be_bytes());
        pack_base.extend_from_slice(&1u32.to_be_bytes());
        let off_base = append_whole_blob(&mut pack_base, base);
        append_sha1_pack_trailer(&mut pack_base);

        let mut pack_tip = Vec::new();
        pack_tip.extend_from_slice(b"PACK");
        pack_tip.extend_from_slice(&2u32.to_be_bytes());
        pack_tip.extend_from_slice(&1u32.to_be_bytes());
        let off_tip = pack_tip.len() as u64;
        append_ref_delta(&mut pack_tip, &oid_base, &delta);
        append_sha1_pack_trailer(&mut pack_tip);

        install_synthetic_pack(&objects, "base", &pack_base, &[(oid_base, off_base)]);
        install_synthetic_pack(&objects, "tip", &pack_tip, &[(oid_target, off_tip)]);

        let got = odb
            .read(&oid_target)
            .expect("Odb::read cross-pack ref-delta");
        assert_eq!(got.kind, ObjectKind::Blob);
        assert_eq!(got.data.as_slice(), target);
        assert_eq!(odb.hash(got.kind, &got.data).expect("hash"), oid_target);
    }

    #[test]
    fn corrupt_in_pack_base_rescued_from_loose() {
        let _guard = pack_cache_test_guard();
        let tmp = tempfile::tempdir().expect("tempdir");
        let objects = tmp.path().join("objects");
        std::fs::create_dir_all(objects.join("pack")).expect("dirs");
        let odb = Odb::new(&objects);
        let base = b"good-base-bytes";
        let tip = b"good-base-bytes!";
        let oid_base = odb.write_local(ObjectKind::Blob, base).expect("loose base");
        let delta = encode_lcp_delta(base, tip).expect("delta");
        let oid_tip = odb.hash(ObjectKind::Blob, tip).expect("hash");

        let mut pack = Vec::new();
        pack.extend_from_slice(b"PACK");
        pack.extend_from_slice(&2u32.to_be_bytes());
        pack.extend_from_slice(&2u32.to_be_bytes());
        let off_base = append_corrupt_whole_blob(&mut pack);
        let off_tip = pack.len() as u64;
        append_ref_delta(&mut pack, &oid_base, &delta);
        append_sha1_pack_trailer(&mut pack);

        let pack_path = objects.join("pack/rescue.pack");
        std::fs::write(&pack_path, &pack).expect("write pack");
        let idx = pack_index_for_objects(pack_path, &[(oid_base, off_base), (oid_tip, off_tip)]);

        let got = read_object_from_pack(&idx, &oid_tip).expect("rescue read");
        assert_eq!(got.kind, ObjectKind::Blob);
        assert_eq!(got.data, tip);
    }

    #[test]
    fn pack_added_after_first_lookup_found_on_miss() {
        let _guard = pack_cache_test_guard();
        clear_pack_cache();
        let tmp = tempfile::tempdir().expect("tempdir");
        let objects = tmp.path().join("objects");
        let pack_dir = objects.join("pack");
        let (pack_a, oid_a, off_a) = single_blob_pack(b"pack-a");
        install_synthetic_pack(&objects, "first", &pack_a, &[(oid_a, off_a)]);
        pack_cache::test_reset_dir_rescan_count(&pack_dir);

        read_object_from_packs(&objects, &oid_a).expect("warm cache");
        assert_eq!(
            pack_cache::test_dir_rescan_count(&pack_dir),
            1,
            "initial lookup should scan the pack directory once"
        );

        // Ensure the pack directory mtime advances past the cached stamp (second resolution).
        std::thread::sleep(std::time::Duration::from_secs(1));

        let (pack_b, oid_b, off_b) = single_blob_pack(b"pack-b");
        install_synthetic_pack_no_clear(&objects, "second", &pack_b, &[(oid_b, off_b)]);
        assert_eq!(
            pack_cache::test_dir_rescan_count(&pack_dir),
            1,
            "adding a pack must not rescan until a miss"
        );

        read_object_from_packs(&objects, &oid_b).expect("new pack via miss path");
        assert_eq!(
            pack_cache::test_dir_rescan_count(&pack_dir),
            2,
            "miss path should re-scan when pack dir mtime changed"
        );
    }

    #[test]
    fn known_object_lookup_does_not_rescan() {
        let _guard = pack_cache_test_guard();
        clear_pack_cache();
        let tmp = tempfile::tempdir().expect("tempdir");
        let objects = tmp.path().join("objects");
        let pack_dir = objects.join("pack");
        pack_cache::test_reset_dir_rescan_count(&pack_dir);
        let (pack, oid, off) = single_blob_pack(b"cached-blob");
        install_synthetic_pack(&objects, "only", &pack, &[(oid, off)]);

        read_object_from_packs(&objects, &oid).expect("prime cache");
        let after_first = pack_cache::test_dir_rescan_count(&pack_dir);
        for _ in 0..64 {
            read_object_from_packs(&objects, &oid).expect("repeat read");
        }
        assert_eq!(
            pack_cache::test_dir_rescan_count(&pack_dir),
            after_first,
            "repeated hits must not re-scan the pack directory"
        );
    }

    #[test]
    fn two_repositories_concurrent_reads() {
        use crate::odb::Odb;
        use std::sync::Arc;

        let _guard = pack_cache_test_guard();
        clear_pack_cache();
        let tmp_a = tempfile::tempdir().expect("tempdir a");
        let tmp_b = tempfile::tempdir().expect("tempdir b");
        let objects_a = tmp_a.path().join("objects");
        let objects_b = tmp_b.path().join("objects");
        let pack_a_dir = objects_a.join("pack");
        let pack_b_dir = objects_b.join("pack");
        let (pack_a, oid_a, off_a) = single_blob_pack(b"parallel-a");
        let (pack_b, oid_b, off_b) = single_blob_pack(b"parallel-b");
        install_synthetic_pack(&objects_a, "a", &pack_a, &[(oid_a, off_a)]);
        install_synthetic_pack(&objects_b, "b", &pack_b, &[(oid_b, off_b)]);
        let odb_a = Odb::new(&objects_a);
        let odb_b = Odb::new(&objects_b);
        assert!(
            !Arc::ptr_eq(odb_a.pack_store().unwrap(), odb_b.pack_store().unwrap()),
            "independent Odb opens must not share pack stores"
        );
        pack_cache::test_reset_dir_rescan_count_on_store(odb_a.pack_store().unwrap(), &pack_a_dir);
        pack_cache::test_reset_dir_rescan_count_on_store(odb_b.pack_store().unwrap(), &pack_b_dir);

        let barrier = Arc::new(std::sync::Barrier::new(2));
        let b0 = Arc::clone(&barrier);
        let b1 = Arc::clone(&barrier);
        let odb_a_t = odb_a.clone();
        let odb_b_t = odb_b.clone();
        let id_a = oid_a;
        let id_b = oid_b;

        let h1 = std::thread::spawn(move || {
            b0.wait();
            odb_a_t.read(&id_a).expect("read a");
        });
        let h2 = std::thread::spawn(move || {
            b1.wait();
            odb_b_t.read(&id_b).expect("read b");
        });
        h1.join().expect("thread a");
        h2.join().expect("thread b");

        assert_eq!(
            pack_cache::test_dir_rescan_count_on_store(odb_a.pack_store().unwrap(), &pack_a_dir),
            1
        );
        assert_eq!(
            pack_cache::test_dir_rescan_count_on_store(odb_b.pack_store().unwrap(), &pack_b_dir),
            1
        );
    }

    #[test]
    fn stale_cached_pack_bytes_revalidated_before_corruption_error() {
        let _guard = pack_cache_test_guard();
        clear_pack_cache();
        let tmp = tempfile::tempdir().expect("tempdir");
        let objects = tmp.path().join("objects");
        let (pack, oid, off) = single_blob_pack(b"revalidate-me");
        let idx = install_synthetic_pack(&objects, "good", &pack, &[(oid, off)]);
        read_object_from_pack(&idx, &oid).expect("prime pack bytes cache");

        let good_on_disk = std::fs::read(&idx.pack_path).expect("read pack");
        pack_cache::test_inject_stale_pack_bytes(
            &idx.pack_path,
            PackData::from_owned(b"BAD".to_vec()),
        );
        std::fs::write(&idx.pack_path, &good_on_disk).expect("refresh on-disk pack");
        let got = read_object_from_pack(&idx, &oid).expect("revalidate from disk");
        assert_eq!(got.data, b"revalidate-me");
    }

    #[test]
    fn replaced_pack_same_path_reread_after_invalidation() {
        let _guard = pack_cache_test_guard();
        clear_pack_cache();
        let tmp = tempfile::tempdir().expect("tempdir");
        let objects = tmp.path().join("objects");
        std::fs::create_dir_all(objects.join("pack")).expect("pack dir");
        let pack_path = objects.join("pack/repack-me.pack");
        let idx_path = objects.join("pack/repack-me.idx");
        const OBJECT_OFFSET: u64 = 9000;
        let (oid_a, off_a) =
            write_sparse_single_blob_pack(tmp.path(), &pack_path, b"before-repack", OBJECT_OFFSET)
                .expect("sparse pack a");
        assert!(
            std::fs::metadata(&pack_path).expect("stat").len() > 8192,
            "fixture pack must exceed mmap owned-buffer threshold"
        );
        write_v2_idx_for_test(&idx_path, &pack_path, &[(oid_a, off_a)]).expect("idx a");
        let idx = read_pack_index(&idx_path).expect("parse idx a");
        read_object_from_pack(&idx, &oid_a).expect("prime cache");

        let (oid_b, off_b) = write_sparse_single_blob_pack(
            tmp.path(),
            &pack_path,
            b"after-repack-content",
            OBJECT_OFFSET,
        )
        .expect("sparse pack b");
        write_v2_idx_for_test(&idx_path, &pack_path, &[(oid_b, off_b)]).expect("idx b");

        assert!(
            pack_cache::revalidate_stale_pack_bytes(&pack_path).expect("revalidate"),
            "replacement must invalidate cached bytes"
        );
        let idx = read_pack_index(&idx_path).expect("reload idx after pack swap");
        let got = read_object_from_pack(&idx, &oid_b).expect("read from replaced pack");
        assert_eq!(got.data, b"after-repack-content");
    }

    #[test]
    fn read_object_at_large_pack_offset_via_mmap() {
        use std::fs::OpenOptions;
        use std::io::{Seek, SeekFrom, Write};

        let _guard = pack_cache_test_guard();
        clear_pack_cache();
        let tmp = tempfile::tempdir().expect("tempdir");
        let objects = tmp.path().join("objects");
        std::fs::create_dir_all(objects.join("pack")).expect("pack dir");

        let data = b"large-offset-blob";
        let odb = Odb::new(tmp.path());
        let oid = odb.hash(ObjectKind::Blob, data).expect("hash");

        let pack_path = objects.join("pack/large-off.pack");
        let mut file = OpenOptions::new()
            .create(true)
            .write(true)
            .read(true)
            .open(&pack_path)
            .expect("create sparse pack");
        file.write_all(b"PACK").expect("sig");
        file.write_all(&2u32.to_be_bytes()).expect("ver");
        file.write_all(&1u32.to_be_bytes()).expect("count");
        let object_offset: u64 = 1 << 32;
        file.seek(SeekFrom::Start(object_offset))
            .expect("seek past 4GiB");
        let mut tail = Vec::new();
        append_whole_blob(&mut tail, data);
        file.write_all(&tail).expect("object at huge offset");
        let trailer = sha1_trailer_for_open_file(&mut file).expect("trailer hash");
        file.write_all(trailer.as_bytes()).expect("append trailer");
        file.flush().expect("flush");
        let file_len = file.metadata().expect("meta").len();
        drop(file);
        assert!(file_len > object_offset);

        let idx_path = pack_path.with_extension("idx");
        write_v2_pack_index_with_trailer(
            &idx_path,
            &[(oid, object_offset, 0)],
            trailer.as_bytes(),
            20,
            None,
        )
        .expect("idx");

        let idx = read_pack_index(&idx_path).expect("parse idx");
        assert_eq!(idx.offset_at(0).expect("off"), object_offset);
        let got = read_object_from_pack(&idx, &oid).expect("read at large offset");
        assert_eq!(got.data, data);
    }

    fn synthetic_index_entries(n: u32) -> Vec<(ObjectId, u64, u32)> {
        (0..n)
            .map(|i| {
                let digest = HashAlgo::Sha1.checksum_digest(&i.to_be_bytes());
                let oid = ObjectId::from_bytes(&digest).expect("oid");
                let offset = if i % 7 == 0 {
                    (1u64 << 31) + u64::from(i)
                } else {
                    12 + u64::from(i) * 40
                };
                (oid, offset, i.wrapping_mul(2_654_435_761))
            })
            .collect()
    }

    #[test]
    fn index_entries_sort_in_place_on_any_thread_count() {
        for n in [0_u32, 1, 300, 20_000] {
            let mut reference = synthetic_index_entries(n);
            reference.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
            let mut fanout_ref = [0u32; 256];
            for (oid, _, _) in &reference {
                for slot in &mut fanout_ref[usize::from(oid.as_bytes()[0])..] {
                    *slot += 1;
                }
            }
            for threads in [None, Some(1), Some(2), Some(3), Some(8)] {
                let mut entries = synthetic_index_entries(n);
                let fanout = sort_index_entries(&mut entries, threads);
                assert_eq!(entries, reference, "n={n} threads={threads:?}");
                assert_eq!(fanout, fanout_ref, "n={n} threads={threads:?}");
            }
        }
    }

    #[test]
    fn index_written_in_place_reads_back_every_row_with_large_offsets() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let idx_path = tmp.path().join("p.idx");
        let rows = synthetic_index_entries(20_000);
        let trailer = [0x5a_u8; 20];
        let mut entries = rows.clone();
        write_v2_pack_index_sorting(&idx_path, &mut entries, &trailer, 20, Some(4))
            .expect("write idx");
        let idx = read_pack_index(&idx_path).expect("read idx");
        assert_eq!(idx.len(), rows.len());
        let mut want = rows;
        want.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
        for (pos, (oid, offset, crc)) in want.iter().enumerate() {
            assert_eq!(idx.oid_at(pos), oid.as_bytes(), "row {pos}");
            assert_eq!(idx.offset_at(pos).expect("offset"), *offset, "row {pos}");
            assert_eq!(idx.crc32_at(pos).expect("crc"), *crc, "row {pos}");
        }

        let mut bad = vec![(ObjectId::from_bytes(&[1; 32]).expect("oid"), 12, 0)];
        assert!(matches!(
            write_v2_pack_index_sorting(&idx_path, &mut bad, &trailer, 20, None),
            Err(Error::CorruptObject(_))
        ));
        assert!(matches!(
            write_v2_pack_index_sorting(&idx_path, &mut entries, &trailer[..19], 20, None),
            Err(Error::CorruptObject(_))
        ));
    }

    /// Hash pack bytes already written to `file` (from offset 0 through current EOF) for a SHA-1 trailer.
    fn sha1_trailer_for_open_file(file: &mut std::fs::File) -> Result<ObjectId> {
        use std::io::{Read, Seek, SeekFrom};
        let len = file.metadata().map_err(Error::Io)?.len();
        file.seek(SeekFrom::Start(0)).map_err(Error::Io)?;
        let mut hasher = HashAlgo::Sha1.checksum_hasher();
        let mut buf = [0u8; 64 * 1024];
        let mut left = len;
        while left > 0 {
            let chunk = left.min(buf.len() as u64) as usize;
            let n = file.read(&mut buf[..chunk]).map_err(Error::Io)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            left -= n as u64;
        }
        let digest = hasher.finalize();
        ObjectId::from_bytes(&digest).map_err(|e| Error::InvalidObjectId(format!("{e}")))
    }

    #[test]
    fn cleared_cache_sees_removed_packs_disappear() {
        let _guard = pack_cache_test_guard();
        clear_pack_cache();
        let tmp = tempfile::tempdir().expect("tempdir");
        let objects = tmp.path().join("objects");
        let (pack, oid, off) = single_blob_pack(b"ephemeral");
        install_synthetic_pack(&objects, "gone", &pack, &[(oid, off)]);
        read_object_from_packs(&objects, &oid).expect("warm cache");

        let pack_path = objects.join("pack/gone.pack");
        let idx_path = objects.join("pack/gone.idx");
        std::fs::remove_file(&pack_path).expect("remove pack");
        std::fs::remove_file(&idx_path).expect("remove idx");
        clear_pack_cache();

        let indexes = read_local_pack_indexes_cached(&objects).expect("rescan");
        assert!(
            indexes.is_empty(),
            "after clear, listing should not retain removed packs"
        );
    }

    #[test]
    fn repack_while_sibling_odb_holds_store_reprepares_on_miss() {
        use crate::odb::Odb;

        let _guard = pack_cache_test_guard();
        clear_pack_cache();
        let tmp = tempfile::tempdir().expect("tempdir");
        let objects = tmp.path().join("objects");
        let (pack_old, oid_old, off_old) = single_blob_pack(b"before");
        install_synthetic_pack(&objects, "old", &pack_old, &[(oid_old, off_old)]);
        let odb_a = Odb::new(&objects);
        let odb_b = Odb::new(&objects);
        assert!(
            !std::sync::Arc::ptr_eq(odb_a.pack_store().unwrap(), odb_b.pack_store().unwrap()),
            "sibling Odbs must have distinct pack stores"
        );
        odb_b.read(&oid_old).expect("warm b store");

        std::fs::remove_file(objects.join("pack/old.pack")).expect("rm pack");
        std::fs::remove_file(objects.join("pack/old.idx")).expect("rm idx");
        let (pack_new, oid_new, off_new) = single_blob_pack(b"after");
        install_synthetic_pack(&objects, "new", &pack_new, &[(oid_new, off_new)]);
        filetime::set_file_mtime(objects.join("pack"), filetime::FileTime::now())
            .expect("bump pack dir mtime for reprepare");
        odb_a.invalidate_packs();

        // B's store is unchanged by A's invalidation; a miss on the new oid rescans pack/.
        let got = odb_b.read(&oid_new).expect("b reprepares after miss");
        assert_eq!(got.data.as_slice(), b"after");
    }
}
#[cfg(test)]
mod cached_lookup_tests {
    use super::*;
    use std::fs;
    use std::process::Command;
    use tempfile::TempDir;

    fn git(dir: &Path, args: &[&str]) {
        let out = Command::new("git") // hygiene: git fixture comparison in cfg(test) module
            .current_dir(dir)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .output()
            .expect("spawn git");
        assert!(
            out.status.success(),
            "git {:?}: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn repack_all(dir: &Path) {
        git(dir, &["repack", "-a", "-d"]);
    }

    fn init_repo_with_pack(dir: &Path) -> ObjectId {
        git(dir, &["init", "-q"]);
        std::fs::write(dir.join("blob.txt"), b"pack cached lookup fixture\n").unwrap();
        git(dir, &["add", "blob.txt"]);
        git(dir, &["commit", "-qm", "c"]);
        repack_all(dir);
        let hex = Command::new("git") // hygiene: git fixture comparison in cfg(test) module
            .current_dir(dir)
            .args(["rev-parse", "HEAD^{tree}"])
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .unwrap();
        let tree_hex = String::from_utf8(hex.stdout).unwrap();
        ObjectId::from_hex(tree_hex.trim()).unwrap()
    }

    fn pack_dir(objects: &Path) -> PathBuf {
        objects.join("pack")
    }

    fn v2_idx_path(objects: &Path) -> PathBuf {
        pack_dir(objects)
            .read_dir()
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .find(|p| p.extension().is_some_and(|x| x == "idx"))
            .expect("pack idx")
    }

    /// Replace the repository's v2 `.idx` with an equivalent v1-format index.
    fn convert_pack_idx_to_v1(objects: &Path) -> PathBuf {
        let idx_path = v2_idx_path(objects);
        let parsed = read_pack_index(&idx_path).expect("v2 idx");
        let mut body = Vec::new();
        for slot in parsed.fanout {
            body.extend_from_slice(&slot.to_be_bytes());
        }
        for entry in parsed.iter() {
            body.extend_from_slice(&(entry.offset().expect("off") as u32).to_be_bytes());
            body.extend_from_slice(entry.oid());
        }
        body.extend_from_slice(HashAlgo::Sha1.checksum_digest(&body).as_slice());
        rewrite_test_file(&idx_path, &body);
        clear_pack_cache();
        idx_path
    }

    fn rewrite_test_file(path: &Path, body: &[u8]) {
        let _ = fs::remove_file(path);
        fs::write(path, body).unwrap();
    }

    fn synthetic_fanout_index(entries: &[(Vec<u8>, u64)]) -> PackIndex {
        let mut sorted = entries.to_vec();
        sorted.sort_by(|a, b| a.0.cmp(&b.0));
        let slices: Vec<&[u8]> = sorted.iter().map(|(o, _)| o.as_slice()).collect();
        let fanout = compute_fanout_from_oid_slices(&slices);
        let mut body = Vec::new();
        for f in fanout {
            body.extend_from_slice(&f.to_be_bytes());
        }
        for (oid, off) in sorted {
            body.extend_from_slice(&(off as u32).to_be_bytes());
            body.extend_from_slice(&oid);
        }
        body.extend_from_slice(HashAlgo::Sha1.checksum_digest(&body).as_slice());
        parse_pack_index_bytes(Path::new("synthetic.idx"), body, false).expect("v1 idx")
    }

    #[test]
    fn find_offset_fanout_bucket_first_last_and_miss() {
        let first = vec![0x05u8; 20];
        let mut mid = vec![0x05u8; 20];
        mid[19] = 1;
        let mut last = vec![0x05u8; 20];
        last[19] = 2;
        let miss = vec![0x06u8; 20];
        let idx =
            synthetic_fanout_index(&[(first.clone(), 12), (mid.clone(), 24), (last.clone(), 36)]);

        let oid_first = ObjectId::from_bytes(&first).unwrap();
        let oid_mid = ObjectId::from_bytes(&mid).unwrap();
        let oid_last = ObjectId::from_bytes(&last).unwrap();
        let oid_miss = ObjectId::from_bytes(&miss).unwrap();

        assert_eq!(idx.find_offset(&oid_first).expect("f"), Some(12));
        assert_eq!(idx.find_offset(&oid_mid).expect("m"), Some(24));
        assert_eq!(idx.find_offset(&oid_last).expect("l"), Some(36));
        assert_eq!(idx.find_offset(&oid_miss).expect("miss"), None);
    }

    #[test]
    fn cached_lookup_hit_v2_idx() {
        let _guard = pack_cache_test_guard();
        clear_pack_cache();
        let dir = TempDir::new().unwrap();
        let tree = init_repo_with_pack(dir.path());
        let objects = dir.path().join(".git").join("objects");
        let got = read_object_from_packs(&objects, &tree).expect("read tree from v2 pack");
        assert_eq!(got.kind, ObjectKind::Tree);
        // Second read must hit the in-memory cache (same result, no reparsing).
        let again = read_object_from_packs(&objects, &tree).expect("cached read");
        assert_eq!(again.data, got.data);
    }

    #[test]
    fn cached_lookup_hit_v1_idx() {
        let _guard = pack_cache_test_guard();
        clear_pack_cache();
        let dir = TempDir::new().unwrap();
        let tree = init_repo_with_pack(dir.path());
        let objects = dir.path().join(".git").join("objects");
        let v1 = convert_pack_idx_to_v1(&objects);
        assert!(pack_index_is_v1(&v1));
        let idx = read_pack_index_cached(&v1).expect("parse v1 idx");
        let off = idx
            .find_offset(&tree)
            .expect("fanout lookup in v1 idx")
            .expect("off");
        assert!(off > 0);
        let obj = read_object_from_pack(&idx, &tree).expect("read via v1 index");
        assert_eq!(obj.kind, ObjectKind::Tree);
    }

    #[test]
    fn cached_lookup_miss() {
        let _guard = pack_cache_test_guard();
        clear_pack_cache();
        let dir = TempDir::new().unwrap();
        init_repo_with_pack(dir.path());
        let objects = dir.path().join(".git").join("objects");
        let miss = ObjectId::from_hex("deadbeefdeadbeefdeadbeefdeadbeefdeadbeef").unwrap();
        assert!(read_object_from_packs(&objects, &miss).is_err());
        assert!(
            try_read_object_from_local_packs_cached(&objects, &miss, || {
                Err(Error::ObjectNotFound(miss.to_hex()))
            })
            .unwrap()
            .is_none()
        );
    }

    #[test]
    fn cached_lookup_loose_only() {
        let _guard = pack_cache_test_guard();
        clear_pack_cache();
        let dir = TempDir::new().unwrap();
        git(dir.path(), &["init", "-q"]);
        let out = Command::new("git") // hygiene: git fixture comparison in cfg(test) module
            .current_dir(dir.path())
            .args(["hash-object", "-w", "--stdin"])
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .and_then(|mut c| {
                use std::io::Write;
                c.stdin
                    .as_mut()
                    .unwrap()
                    .write_all(b"loose-only\n")
                    .unwrap();
                c.wait_with_output()
            })
            .unwrap();
        assert!(out.status.success());
        let oid = ObjectId::from_hex(String::from_utf8(out.stdout).unwrap().trim()).unwrap();
        let objects = dir.path().join(".git").join("objects");
        assert!(try_read_object_from_local_packs_cached(&objects, &oid, || {
            Err(Error::ObjectNotFound(oid.to_hex()))
        })
        .unwrap()
        .is_none());
    }

    #[test]
    fn cached_lookup_multiple_packs() {
        let _guard = pack_cache_test_guard();
        clear_pack_cache();
        let dir = TempDir::new().unwrap();
        git(dir.path(), &["init", "-q"]);
        std::fs::write(dir.path().join("a.txt"), b"a").unwrap();
        git(dir.path(), &["add", "a.txt"]);
        git(dir.path(), &["commit", "-qm", "a"]);
        repack_all(dir.path());
        let oid_a = ObjectId::from_hex(
            String::from_utf8(
                Command::new("git") // hygiene: git fixture comparison in cfg(test) module // hygiene: git fixture comparison in cfg(test) module
                    .current_dir(dir.path())
                    .args(["rev-parse", "HEAD:a.txt"])
                    .env("GIT_CONFIG_GLOBAL", "/dev/null")
                    .env("GIT_CONFIG_SYSTEM", "/dev/null")
                    .output()
                    .unwrap()
                    .stdout,
            )
            .unwrap()
            .trim(),
        )
        .unwrap();
        std::fs::write(dir.path().join("b.txt"), b"b").unwrap();
        git(dir.path(), &["add", "b.txt"]);
        git(dir.path(), &["commit", "-qm", "b"]);
        git(dir.path(), &["repack", "-d"]);
        let oid_b = ObjectId::from_hex(
            String::from_utf8(
                Command::new("git") // hygiene: git fixture comparison in cfg(test) module // hygiene: git fixture comparison in cfg(test) module
                    .current_dir(dir.path())
                    .args(["rev-parse", "HEAD:b.txt"])
                    .env("GIT_CONFIG_GLOBAL", "/dev/null")
                    .env("GIT_CONFIG_SYSTEM", "/dev/null")
                    .output()
                    .unwrap()
                    .stdout,
            )
            .unwrap()
            .trim(),
        )
        .unwrap();
        let objects = dir.path().join(".git").join("objects");
        let pack_count = pack_dir(&objects)
            .read_dir()
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().is_some_and(|x| x == "pack"))
            .count();
        assert!(
            pack_count >= 2,
            "expected multiple pack files, got {pack_count}"
        );
        let obj_a = read_object_from_packs(&objects, &oid_a).expect("pack a");
        let obj_b = read_object_from_packs(&objects, &oid_b).expect("pack b");
        assert_eq!(obj_a.kind, ObjectKind::Blob);
        assert_eq!(obj_b.kind, ObjectKind::Blob);
    }

    #[test]
    fn packed_delta_base_oid_uses_cached_index() {
        let _guard = pack_cache_test_guard();
        clear_pack_cache();
        let dir = TempDir::new().unwrap();
        init_repo_with_pack(dir.path());
        let objects = dir.path().join(".git").join("objects");
        let miss = ObjectId::from_hex("0101010101010101010101010101010101010101").unwrap();
        assert!(packed_delta_base_oid(&objects, &miss).unwrap().is_none());
    }

    #[test]
    fn content_addressed_pack_bytes_skip_stat_revalidation() {
        let _guard = pack_cache_test_guard();
        clear_pack_cache();
        let dir = tempfile::tempdir().expect("tempdir");
        let pack_path = dir
            .path()
            .join("pack-1111111111111111111111111111111111111111.pack");
        let v1 = b"PACK\x00\x00\x00\x02\x00\x00\x00\x00";
        std::fs::write(&pack_path, v1).expect("write v1");
        let cached = read_pack_bytes_cached(&pack_path).expect("prime cache");
        assert_eq!(&cached[..], v1);

        let v2 = b"PACK-replaced-by-repack";
        std::fs::write(&pack_path, v2).expect("overwrite on disk");
        let again = read_pack_bytes_cached(&pack_path).expect("cached without stat");
        assert_eq!(
            &again[..],
            v1,
            "content-addressed name pins cached bytes in-process"
        );
    }

    #[test]
    fn temporary_pack_bytes_served_from_cache_until_cleared() {
        let _guard = pack_cache_test_guard();
        clear_pack_cache();
        let dir = tempfile::tempdir().expect("tempdir");
        for pack_path in [
            dir.path().join("tmp_pack_abc123.pack"),
            dir.path().join("pack-custom.pack"),
        ] {
            clear_pack_cache();
            let v1 = b"PACK-temp-v1";
            std::fs::write(&pack_path, v1).expect("write v1");
            let _ = read_pack_bytes_cached(&pack_path).expect("prime cache");

            let v2 = b"PACK-temp-v2-longer-body";
            std::fs::write(&pack_path, v2).expect("overwrite pack");
            let stale = read_pack_bytes_cached(&pack_path).expect("cache hit");
            assert_eq!(
                &stale[..],
                v1,
                "{} serves cached bytes without stat on hit",
                pack_path.display()
            );
            clear_pack_cache();
            let fresh = read_pack_bytes_cached(&pack_path).expect("after clear");
            assert_eq!(
                &fresh[..],
                v2,
                "{} reloads from disk after clear",
                pack_path.display()
            );
        }
    }
    #[test]
    fn system_git_repack_requires_clear_pack_cache_for_fresh_listing() {
        let _guard = pack_cache_test_guard();
        clear_pack_cache();
        let dir = TempDir::new().unwrap();
        init_repo_with_pack(dir.path());
        let objects = dir.path().join(".git").join("objects");
        read_local_pack_indexes_cached(&objects).expect("warm listing");
        for entry in pack_dir(&objects).read_dir().unwrap().flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "pack" || e == "idx") {
                fs::remove_file(path).unwrap();
            }
        }
        clear_pack_cache();
        let indexes =
            read_local_pack_indexes_cached(&objects).expect("list after repack-like delete");
        assert!(
            indexes.is_empty(),
            "callers must clear_pack_cache after removing pack files"
        );
    }

    fn test_oid(n: u8) -> ObjectId {
        ObjectId::from_hex(&format!("{n:040x}")).unwrap()
    }

    fn max_delta_map_chain_edges(map: &HashMap<ObjectId, ObjectId>) -> usize {
        let value_set: HashSet<ObjectId> = map.values().copied().collect();
        let tips: Vec<ObjectId> = map
            .keys()
            .copied()
            .filter(|k| !value_set.contains(k))
            .collect();
        let mut max = 0usize;
        for tip in tips {
            let mut edges = 0usize;
            let mut cur = tip;
            let mut seen = HashSet::new();
            while seen.insert(cur) {
                let Some(&b) = map.get(&cur) else {
                    break;
                };
                edges += 1;
                cur = b;
            }
            max = max.max(edges);
        }
        max
    }

    #[test]
    fn apply_delta_depth_limit_hits_exact_cap_on_seventy_edge_chain() {
        let n = 70usize;
        let oids: Vec<ObjectId> = (0..n as u8).map(test_oid).collect();
        let mut map: HashMap<ObjectId, ObjectId> = HashMap::new();
        for i in 1..n {
            map.insert(oids[i], oids[i - 1]);
        }
        let original = map.clone();
        apply_delta_depth_limit(&mut map, 50);
        assert_eq!(max_delta_map_chain_edges(&map), 50);
        let preserved = map
            .iter()
            .filter(|(t, b)| original.get(t) == Some(b))
            .count();
        assert!(
            preserved > 40,
            "non-cut edges should keep original bases (preserved {preserved})"
        );
    }

    #[test]
    fn apply_delta_depth_limit_leaves_short_chains_untouched() {
        let a = test_oid(1);
        let b = test_oid(2);
        let c = test_oid(3);
        let mut map = HashMap::from([(b, a), (c, b)]);
        apply_delta_depth_limit(&mut map, 50);
        assert_eq!(map.get(&b), Some(&a));
        assert_eq!(map.get(&c), Some(&b));
        assert_eq!(max_delta_map_chain_edges(&map), 2);
    }

    #[test]
    fn apply_delta_depth_limit_caps_long_chain_and_preserves_blobs() {
        let n = 120usize;
        let oids: Vec<ObjectId> = (0..n as u8).map(test_oid).collect();
        let mut blobs: HashMap<ObjectId, Vec<u8>> = HashMap::new();
        blobs.insert(oids[0], b"base".to_vec());
        for i in 1..n {
            let mut next = blobs[&oids[i - 1]].clone();
            next.push(i as u8);
            blobs.insert(oids[i], next);
        }
        let mut map: HashMap<ObjectId, ObjectId> = HashMap::new();
        for i in 1..n {
            map.insert(oids[i], oids[i - 1]);
        }
        let original = map.clone();
        apply_delta_depth_limit(&mut map, 50);
        assert_eq!(
            max_delta_map_chain_edges(&map),
            50,
            "long chain must cap at exactly max_depth edges"
        );
        let preserved = map
            .iter()
            .filter(|(target, base)| original.get(target) == Some(base))
            .count();
        assert!(
            preserved > 60,
            "segment links should stay on original bases (preserved {preserved} of {})",
            map.len()
        );

        use crate::delta_encode::encode_prefix_extension_delta;
        use crate::unpack_objects::apply_delta;

        for (&target, &base) in &map {
            let delta = encode_prefix_extension_delta(&blobs[&base], &blobs[&target]).unwrap();
            let got = apply_delta(&blobs[&base], &delta).unwrap();
            assert_eq!(got, blobs[&target]);
        }
    }

    #[test]
    fn v2_idx_crc_matches_crc32fast_over_entry_bytes() {
        let _guard = pack_cache_test_guard();
        clear_pack_cache();
        let dir = TempDir::new().unwrap();
        let tree = init_repo_with_pack(dir.path());
        let objects = dir.path().join(".git").join("objects");
        let idx_path = v2_idx_path(&objects);
        let idx = read_pack_index(&idx_path).expect("read idx");
        let pack_bytes = std::fs::read(&idx.pack_path).expect("read pack");
        for entry in idx.iter() {
            let crc = entry.crc32().expect("v2 idx entry crc");
            let off = entry.offset().expect("entry off");
            let start = off as usize;
            let end = super::pack_entry_raw_end(&idx, &pack_bytes, off)
                .expect("entry end")
                .expect("end");
            assert_eq!(crc32fast::hash(&pack_bytes[start..end]), crc);
        }
        let _ = tree;
    }

    #[test]
    fn packed_full_object_slice_returns_on_disk_bytes() {
        let _guard = pack_cache_test_guard();
        clear_pack_cache();
        let dir = TempDir::new().unwrap();
        let tree = init_repo_with_pack(dir.path());
        let objects = dir.path().join(".git").join("objects");
        let idx = read_pack_index(&v2_idx_path(&objects)).expect("idx");
        let pack_bytes = std::fs::read(&idx.pack_path).expect("pack");
        let off = idx.find_offset(&tree).expect("tree in pack").expect("off");
        let start = off as usize;
        let end = super::pack_entry_raw_end(&idx, &pack_bytes, off)
            .expect("entry end")
            .expect("end");
        let expected = &pack_bytes[start..end];
        let slice = packed_full_object_slice(&objects, &tree)
            .expect("lookup")
            .expect("full tree slice");
        assert_eq!(slice, expected);
    }

    fn touch_mtime(path: &Path, when: filetime::FileTime) {
        filetime::set_file_mtime(path, when).expect("set mtime");
    }

    #[test]
    fn packed_full_object_slice_crc_mismatch_falls_back_to_redundant_pack() {
        let _guard = pack_cache_test_guard();
        clear_pack_cache();
        let dir = TempDir::new().unwrap();
        let tree = init_repo_with_pack(dir.path());
        let objects = dir.path().join(".git").join("objects");
        let pack_dir = pack_dir(&objects);
        let primary_idx = read_pack_index(&v2_idx_path(&objects)).expect("idx");
        let primary_pack = primary_idx.pack_path.clone();
        let primary_idx_path = primary_idx.idx_path.clone();

        let redundant_pack = pack_dir.join("pack-redundant.pack");
        let redundant_idx = pack_dir.join("pack-redundant.idx");
        fs::copy(&primary_pack, &redundant_pack).expect("copy pack");
        fs::copy(&primary_idx_path, &redundant_idx).expect("copy idx");

        let older = filetime::FileTime::from_unix_time(1_700_000_000, 0);
        let newer = filetime::FileTime::from_unix_time(1_700_000_100, 0);
        touch_mtime(&primary_pack, older);
        touch_mtime(&primary_idx_path, older);
        touch_mtime(&redundant_pack, newer);
        touch_mtime(&redundant_idx, newer);
        clear_pack_cache();

        let off = primary_idx
            .find_offset(&tree)
            .expect("tree offset")
            .expect("off");
        let start = off as usize;
        let end = super::pack_entry_raw_end(&primary_idx, &fs::read(&primary_pack).unwrap(), off)
            .expect("entry end")
            .expect("end");
        let flip = start + (end - start) / 2;
        let mut corrupt = fs::read(&redundant_pack).expect("read redundant");
        corrupt[flip] ^= 0x01;
        rewrite_test_file(&redundant_pack, &corrupt);
        clear_pack_cache();

        let recorded_crc = read_pack_index(&redundant_idx)
            .expect("redundant idx")
            .crc32_for_pack_offset(off)
            .expect("crc")
            .expect("crc value");
        let corrupt_slice = &corrupt[start..end];
        assert_ne!(
            crc32fast::hash(corrupt_slice),
            recorded_crc,
            "test must flip bytes without updating idx CRC"
        );
        assert!(
            packed_full_object_slice(&objects, &tree)
                .expect("lookup")
                .is_some(),
            "valid redundant pack should serve the slice after CRC rejects corrupt copy"
        );

        let obj = read_object_from_packs(&objects, &tree).expect("read tree");
        assert_eq!(obj.kind, ObjectKind::Tree);
        let slice = packed_full_object_slice(&objects, &tree)
            .expect("lookup")
            .expect("slice");
        let good_pack = fs::read(&primary_pack).expect("primary pack");
        assert_eq!(&slice, &good_pack[start..end]);
    }
}
