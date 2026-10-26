//! Filesystem ref backend (loose refs + `packed-refs`) implementing [`super::RefStore`].

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs;
use std::io;
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::error::{Error, Result};
use crate::git_dir::io_error::{io_with_git_path, map_git_dir_io, GitDirPathOperation};
use crate::lockfile::LockWait;
use crate::objects::ObjectId;
use crate::reflog::{self, ReflogEntry};
use crate::refs::{
    lock_path_for_ref, loose_ref_list_walk_root, packed_record_name, packed_record_oid,
    prune_empty_ref_dirs, read_packed_refs_file_view, read_packed_refs_map_for_prefix_filtered,
    read_ref_file, remove_empty_ref_directory, remove_packed_ref, remove_packed_ref_under_lock,
    remove_packed_refs_under_lock, restore_packed_records_under_lock, PackedPeel,
    PackedRecordFilter, Ref,
};

use super::apply::{
    apply_case_only_ref_rename, apply_reflog_rename, apply_update, ref_change_target,
    ref_lock_names_for_updates, simulate_batch_apply, RefBatchState,
};
use super::error::RefStoreError;
use super::routing::{resolve_common_dir, route_ref_storage_in_common, RefStorageRoute};
use super::transaction::expected_matches;
use super::transaction::RefTransaction;
use super::types::Expected;
use super::validation::{
    validate_storable_refname, verify_create_conflicts, verify_name_available_for_create,
};
use super::{PreparedRefTransaction, RawRef, RefEntry, RefStorageFormat, RefStore, RefUpdate};
use crate::refs::RefnameUnavailable;

type StoreResult<T> = std::result::Result<T, RefStoreError>;

/// Explicit context for a files ref store (no environment reads for namespace).
#[derive(Debug, Clone)]
pub struct FilesRefStoreConfig {
    /// Worktree or bare repository git directory.
    pub git_dir: PathBuf,
    /// Shared object/ref store (`commondir` target or same as `git_dir`).
    pub common_dir: PathBuf,
    /// Optional `GIT_NAMESPACE`-style prefix applied to storage paths (not read from env).
    pub namespace_prefix: Option<String>,
}

/// Loose + packed ref storage on disk.
#[derive(Debug)]
pub struct FilesRefStore {
    git_dir: PathBuf,
    common_dir: PathBuf,
    namespace_prefix: Option<String>,
    packed_cache: Mutex<PackedCacheEntry>,
    prepared: Mutex<Option<PreparedFilesBatch>>,
}

/// What a cached `packed-refs` view was read from. Writers replace the file by rename, so the
/// file identity changes even when the new file has the same length and mtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PackedRefsStamp {
    len: u64,
    modified: Option<std::time::SystemTime>,
    /// Device, inode and status-change time (seconds, nanoseconds), where the platform has them.
    identity: Option<(u64, u64, i64, i64)>,
}

#[derive(Debug)]
struct PackedCacheEntry {
    stamp: PackedRefsStamp,
    loaded: bool,
    file: Option<crate::refs::PackedRefsFileView>,
    /// Full packed-refs map; only valid when [`Self::map_is_complete`].
    map: Option<BTreeMap<String, ObjectId>>,
    map_is_complete: bool,
}

/// Per-batch state shared by the steps of [`FilesRefStore::persist_state_diff`].
#[derive(Debug, Clone, Copy)]
struct CommitContext<'a> {
    /// How long to wait for each loose ref lock, read from config once per batch.
    ref_lock_wait: LockWait,
    /// Whether the batch holds `packed-refs.lock`.
    lock_held: bool,
    /// Logical names whose packed records were already dropped in one up-front rewrite.
    pruned: &'a HashSet<String>,
    /// Values of the records that rewrite removed, so commit-time rechecks still see them.
    removed: &'a HashMap<String, RawRef>,
    /// Directories that deletions may have left empty (mapped to their storage directory),
    /// pruned once the batch is done instead of after every ref.
    emptied_dirs: &'a RefCell<BTreeMap<PathBuf, PathBuf>>,
}

/// What [`FilesRefStore::prune_deleted_packed_records`] took out of `packed-refs`.
#[derive(Debug, Default)]
struct PrunedPackedRecords {
    /// Logical names of every ref the batch deletes.
    names: HashSet<String>,
    /// Removed values of deletions whose expectation the commit-time recheck verifies.
    removed: HashMap<String, RawRef>,
    /// Removed records verbatim, per storage directory.
    records: Vec<(PathBuf, Vec<String>)>,
}

#[derive(Debug)]
struct PreparedFilesBatch {
    updates: Vec<RefUpdate>,
    case_only_renames: Vec<super::RefCaseOnlyRename>,
    ref_locks: HashMap<String, PathBuf>,
    packed_lock: Option<PathBuf>,
    ref_lock_wait: LockWait,
    /// Ref/reflog state observed at prepare (must drive commit, not a reload).
    before_commit: RefBatchState,
    after_commit: RefBatchState,
}

impl FilesRefStore {
    /// Open a files backend at `config.git_dir`.
    #[must_use]
    pub fn open(config: FilesRefStoreConfig) -> Self {
        let path = super::paths::packed_refs_path(&config.common_dir);
        let stamp = Self::packed_refs_stamp(&path);
        Self {
            git_dir: config.git_dir,
            common_dir: config.common_dir,
            namespace_prefix: config.namespace_prefix,
            packed_cache: Mutex::new(PackedCacheEntry {
                stamp,
                loaded: false,
                file: None,
                map: None,
                map_is_complete: false,
            }),
            prepared: Mutex::new(None),
        }
    }

    /// Git directory this store reads `HEAD` and per-worktree refs from.
    pub(crate) fn git_dir(&self) -> &Path {
        &self.git_dir
    }

    /// Convenience constructor for a simple repository directory.
    pub fn from_git_dir(git_dir: impl Into<PathBuf>) -> Result<Self> {
        let git_dir: PathBuf = git_dir.into();
        let git_dir = fs::canonicalize(&git_dir).unwrap_or(git_dir);
        let common_dir = resolve_common_dir(&git_dir);
        Ok(Self::open(FilesRefStoreConfig {
            git_dir: git_dir.clone(),
            common_dir,
            namespace_prefix: crate::ref_namespace::ref_storage_prefix_default(),
        }))
    }

    fn packed_refs_stamp(path: &Path) -> PackedRefsStamp {
        match fs::metadata(path) {
            Ok(meta) => PackedRefsStamp {
                len: meta.len(),
                modified: meta.modified().ok(),
                identity: file_identity(&meta),
            },
            Err(_) => PackedRefsStamp {
                len: 0,
                modified: None,
                identity: None,
            },
        }
    }

    /// Drop the cached `packed-refs` view so the next lookup reads the file again.
    ///
    /// Stamps can miss a replacement on filesystems with coarse timestamps or reused inodes,
    /// so checks whose answer must reflect the file right now (the create-conflict recheck
    /// under the ref lock) start from a fresh read.
    fn forget_packed_cache(&self) {
        let mut guard = self.packed_cache.lock().unwrap_or_else(|e| e.into_inner());
        guard.loaded = false;
        guard.file = None;
        guard.map = None;
        guard.map_is_complete = false;
    }

    fn invalidate_packed_cache_after_write(&self) {
        let mut guard = self.packed_cache.lock().unwrap_or_else(|e| e.into_inner());
        guard.map = None;
        guard.map_is_complete = false;
    }

    fn refresh_packed_cache_if_stale(&self) -> Result<()> {
        let path = super::paths::packed_refs_path(&self.common_dir);
        let mut guard = self.packed_cache.lock().unwrap_or_else(|e| e.into_inner());
        let stamp = Self::packed_refs_stamp(&path);
        if guard.loaded && guard.stamp == stamp {
            return Ok(());
        }
        guard.file = read_packed_refs_file_view(&self.common_dir)?;
        guard.map = None;
        guard.map_is_complete = false;
        guard.stamp = stamp;
        guard.loaded = true;
        Ok(())
    }

    fn ensure_full_packed_map(&self) -> Result<()> {
        self.refresh_packed_cache_if_stale()?;
        let mut guard = self.packed_cache.lock().unwrap_or_else(|e| e.into_inner());
        if guard.map.is_some() && guard.map_is_complete {
            return Ok(());
        }
        let map = if let Some(ref file) = guard.file {
            let path = super::paths::packed_refs_path(&self.common_dir);
            file.map_for_prefix(&path, None)?
                .into_iter()
                .collect::<BTreeMap<_, _>>()
        } else {
            BTreeMap::new()
        };
        guard.map = Some(map);
        guard.map_is_complete = true;
        Ok(())
    }

    fn first_packed_descendant(&self, storage_name: &str) -> Result<Option<(String, ObjectId)>> {
        self.refresh_packed_cache_if_stale()?;
        let guard = self.packed_cache.lock().unwrap_or_else(|e| e.into_inner());
        let Some(ref file) = guard.file else {
            return Ok(None);
        };
        let path = super::paths::packed_refs_path(&self.common_dir);
        file.first_descendant(&path, storage_name)
    }

    /// Records of the common dir's `packed-refs` under `prefix` (storage names), read through
    /// the cached file view so a listing and later lookups index the file once.
    fn packed_map_for_prefix_filtered(
        &self,
        prefix: &str,
        keep: Option<PackedRecordFilter<'_>>,
    ) -> Result<HashMap<String, ObjectId>> {
        self.refresh_packed_cache_if_stale()?;
        let guard = self.packed_cache.lock().unwrap_or_else(|e| e.into_inner());
        let Some(ref file) = guard.file else {
            return Ok(HashMap::new());
        };
        let path = super::paths::packed_refs_path(&self.common_dir);
        let prefix = (!prefix.is_empty()).then_some(prefix);
        file.map_for_prefix_filtered(&path, prefix, keep)
    }

    /// The common dir's `packed-refs` view this store reads through, refreshed when the file
    /// changed (`None` without a `packed-refs`). The copy shares the file's bytes and index.
    pub(crate) fn packed_refs_view(&self) -> Result<Option<crate::refs::PackedRefsFileView>> {
        self.refresh_packed_cache_if_stale()?;
        let guard = self.packed_cache.lock().unwrap_or_else(|e| e.into_inner());
        Ok(guard.file.clone())
    }

    fn lookup_packed_oid(&self, storage_name: &str) -> Result<Option<ObjectId>> {
        self.refresh_packed_cache_if_stale()?;
        let guard = self.packed_cache.lock().unwrap_or_else(|e| e.into_inner());
        let Some(ref file) = guard.file else {
            return Ok(None);
        };
        let path = super::paths::packed_refs_path(&self.common_dir);
        file.lookup_oid(&path, storage_name)
    }

    /// Add to `refs` every existing ref that can block creating a new ref of `updates`: its
    /// ancestors (`refs/heads/a` for `refs/heads/a/b`) and the refs under `<name>/`.
    ///
    /// `refs` must already hold every update target that exists. With these neighbors,
    /// [`verify_create_conflicts`] reaches the same verdict as against the whole store, so a
    /// create costs a few lookups instead of reading every ref and reflog.
    fn insert_create_conflict_neighbors(
        &self,
        refs: &mut BTreeMap<String, RawRef>,
        updates: &[RefUpdate],
    ) -> Result<()> {
        let creates: Vec<&str> = updates
            .iter()
            .filter(|u| u.new_value.is_some() && !u.flags.log_only && !refs.contains_key(&u.name))
            .map(|u| u.name.as_str())
            .collect();
        for name in creates {
            for (slash, _) in name.match_indices('/') {
                let ancestor = &name[..slash];
                if refs.contains_key(ancestor) {
                    continue;
                }
                if let Some(raw) = self.read_existing_ref_named(ancestor)? {
                    refs.insert(ancestor.to_owned(), raw);
                }
            }
            for (descendant, raw) in self.descendant_refs(name)? {
                refs.entry(descendant).or_insert(raw);
            }
        }
        Ok(())
    }

    /// Whether `name` can be created next to the refs on disk and the other names of its batch
    /// (`extras`), under the rules of [`crate::refs::verify_refname_available_for_create`].
    ///
    /// Lookups go through this store's packed-refs cache, which revalidates against the file's
    /// size and mtime, instead of re-reading `packed-refs` for every ancestor. Any file under
    /// `<name>/` blocks the create, since the directory could not be replaced by the ref file.
    /// Unreadable refs count as absent, as in the path-based check.
    fn verify_create_available(
        &self,
        name: &str,
        extras: &BTreeSet<String>,
    ) -> std::result::Result<(), RefnameUnavailable> {
        let mut blocking = BTreeSet::new();
        for (slash, _) in name.match_indices('/') {
            let ancestor = &name[..slash];
            if let Ok(Some(_)) = self.read_existing_ref_named(ancestor) {
                blocking.insert(ancestor.to_owned());
            }
        }
        let storage = self.route(name).storage_name;
        let mut storage_names = BTreeSet::new();
        for base in self.store_bases() {
            collect_loose_file_names(&base.join(&storage), &storage, &mut storage_names);
        }
        if storage.contains('/') {
            if let Ok(Some((packed, _))) = self.first_packed_descendant(&storage) {
                storage_names.insert(packed);
            }
        }
        blocking.extend(storage_names.iter().map(|s| self.logical_name(s)));
        verify_name_available_for_create(name, &blocking, extras)
    }

    fn store_bases(&self) -> impl Iterator<Item = &Path> {
        let common = self.common_dir.as_path();
        let git = self.git_dir.as_path();
        std::iter::once(common).chain((git != common).then_some(git))
    }

    /// The value of `name` as a full listing would report it: a parseable loose file in either
    /// store directory, else a visible packed record. Directories and unparseable files are
    /// not refs.
    fn read_existing_ref_named(&self, name: &str) -> Result<Option<RawRef>> {
        let storage = self.route(name).storage_name;
        for base in self.store_bases() {
            let path = base.join(&storage);
            if !path.is_file() {
                continue;
            }
            if let Ok(r) = read_ref_file(&path) {
                return Ok(Some(ref_to_raw(r)));
            }
        }
        if !self.packed_storage_visible(&storage) {
            return Ok(None);
        }
        Ok(self.lookup_packed_oid(&storage)?.map(RawRef::Direct))
    }

    /// Loose refs under `<name>/` in either store directory, plus the first packed one.
    fn descendant_refs(&self, name: &str) -> Result<BTreeMap<String, RawRef>> {
        let storage = self.route(name).storage_name;
        let mut storage_map = BTreeMap::new();
        for base in self.store_bases() {
            let dir = base.join(&storage);
            if dir.is_dir() {
                collect_loose_raw_refs(&dir, &format!("{storage}/"), "", &mut storage_map)?;
            }
        }
        // Refs live under `refs/`, so a one-level name has no packed descendants; skipping it
        // also avoids scanning from a pseudoref like `ORIG_HEAD` to the end of the file.
        if storage.contains('/') {
            if let Some((packed, oid)) = self.first_packed_descendant(&storage)? {
                if self.packed_storage_visible(&packed) {
                    storage_map.entry(packed).or_insert(RawRef::Direct(oid));
                }
            }
        }
        Ok(storage_map
            .into_iter()
            .map(|(storage, value)| (self.logical_name(&storage), value))
            .collect())
    }

    /// Load `start` and any further symbolic targets into `refs` (up to [`crate::refs::SYMREF_MAXDEPTH`]).
    fn insert_symref_peel_chain(
        &self,
        refs: &mut BTreeMap<String, RawRef>,
        start: &str,
    ) -> Result<()> {
        let mut name = start.to_owned();
        for _ in 0..crate::refs::SYMREF_MAXDEPTH {
            if refs.contains_key(&name) {
                match refs.get(&name) {
                    Some(RawRef::Symbolic(next)) => name = next.clone(),
                    _ => break,
                }
                continue;
            }
            let Some(raw) = self.read_raw(&name)? else {
                break;
            };
            let advance = match &raw {
                RawRef::Symbolic(next) => Some(next.clone()),
                RawRef::Direct(_) => None,
            };
            refs.insert(name.clone(), raw);
            match advance {
                Some(next) => name = next,
                None => break,
            }
        }
        Ok(())
    }

    fn load_batch_state_for_prepare(
        &self,
        updates: &[RefUpdate],
        reflog_renames: &[super::RefReflogRename],
        case_only_renames: &[super::RefCaseOnlyRename],
    ) -> Result<RefBatchState> {
        let mut refs = BTreeMap::new();
        for update in updates {
            if let Some(raw) = self.read_raw(&update.name)? {
                refs.insert(update.name.clone(), raw);
            }
        }
        for rename in case_only_renames {
            if refs.contains_key(&rename.from) {
                continue;
            }
            if let Some(raw) = self.read_raw(&rename.from)? {
                refs.insert(rename.from.clone(), raw);
            }
        }
        self.insert_head_for_batch_state(&mut refs)?;

        for update in updates {
            if matches!(refs.get(&update.name), Some(RawRef::Symbolic(_))) {
                self.insert_symref_peel_chain(&mut refs, &update.name)?;
            }
        }
        if matches!(refs.get("HEAD"), Some(RawRef::Symbolic(_))) {
            self.insert_symref_peel_chain(&mut refs, "HEAD")?;
        }

        let mut reflogs = self.load_reflogs_for_updates(&refs, updates)?;
        for rename in reflog_renames {
            if reflogs.contains_key(&rename.from) {
                continue;
            }
            let entries = read_reflog_at_path(&self.reflog_path(&rename.from))?;
            if !entries.is_empty() {
                reflogs.insert(rename.from.clone(), entries);
            }
        }
        Ok(RefBatchState { refs, reflogs })
    }

    fn ensure_reflog_path_writable(&self, name: &str) -> StoreResult<()> {
        let path = self.reflog_path(name);
        if path.is_dir() {
            return Err(RefStoreError::Corrupt(format!(
                "refusing to write reflog at '{}': path is a directory",
                path.display()
            )));
        }
        Ok(())
    }

    fn read_raw_at_route(&self, route: &RefStorageRoute) -> Result<Option<RawRef>> {
        let path = route.storage_dir.join(&route.storage_name);
        match read_ref_file(&path) {
            Ok(Ref::Direct(oid)) => return Ok(Some(RawRef::Direct(oid))),
            Ok(Ref::Symbolic(target)) => return Ok(Some(RawRef::Symbolic(target))),
            Err(Error::Io(ref e)) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        if let Some(oid) = self.lookup_packed_oid(&route.storage_name)? {
            return Ok(Some(RawRef::Direct(oid)));
        }
        Ok(None)
    }

    fn packed_storage_visible(&self, storage: &str) -> bool {
        match self.namespace_prefix.as_deref() {
            None => true,
            Some(prefix) => storage.starts_with(prefix),
        }
    }

    /// Prefix for scanning `packed-refs` lines (storage paths, not logical names).
    fn packed_refs_scan_prefix(&self, logical_prefix: &str) -> String {
        match self.namespace_prefix.as_deref() {
            Some(ns) if !logical_prefix.is_empty() => format!("{ns}{logical_prefix}"),
            Some(ns) => ns.trim_end_matches('/').to_owned(),
            None => logical_prefix.to_owned(),
        }
    }

    /// Raw refs whose logical names start with `logical_prefix`, loose values overriding packed
    /// ones. `keep`, given a packed record's storage name, id and peel, may leave it out.
    fn load_raw_map_prefix(
        &self,
        logical_prefix: &str,
        keep: Option<PackedRecordFilter<'_>>,
    ) -> Result<BTreeMap<String, RawRef>> {
        let logical_prefix = logical_prefix.trim_end_matches('/');
        let mut map: BTreeMap<String, RawRef> = BTreeMap::new();
        if logical_prefix.is_empty() {
            self.ensure_full_packed_map()?;
            let cache = self.packed_cache.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(packed) = cache.map.as_ref() {
                for (storage, oid) in packed.iter() {
                    if !self.packed_storage_visible(storage)
                        || keep.is_some_and(|keep| !keep(storage, oid, PackedPeel::Unknown))
                    {
                        continue;
                    }
                    let logical = self.logical_name(storage);
                    map.insert(logical, RawRef::Direct(*oid));
                }
            }
            drop(cache);
        } else {
            let common = self.common_dir.as_path();
            let git = self.git_dir.as_path();
            let bases: [&Path; 2] = if common == git {
                [common, common]
            } else {
                [common, git]
            };
            for (i, base) in bases.iter().enumerate() {
                if i == 1 && *base == bases[0] {
                    continue;
                }
                let packed_prefix = self.packed_refs_scan_prefix(logical_prefix);
                let packed = if i == 0 {
                    self.packed_map_for_prefix_filtered(&packed_prefix, keep)?
                } else {
                    read_packed_refs_map_for_prefix_filtered(base, &packed_prefix, keep)?
                };
                for (storage, oid) in packed {
                    if !self.packed_storage_visible(&storage) {
                        continue;
                    }
                    let logical = self.logical_name(&storage);
                    map.insert(logical, RawRef::Direct(oid));
                }
            }
        }

        for base in [self.common_dir.as_path(), self.git_dir.as_path()] {
            let storage_prefix = self.namespace_prefix.as_deref().unwrap_or("refs/");
            if self.namespace_prefix.is_some() {
                let root = base.join(storage_prefix.trim_end_matches('/'));
                if root.is_dir() {
                    let mut storage_map = BTreeMap::new();
                    collect_loose_raw_refs(&root, storage_prefix, "", &mut storage_map)?;
                    for (storage, value) in storage_map {
                        let logical = self.logical_name(&storage);
                        if logical_prefix.is_empty() || logical.starts_with(logical_prefix) {
                            map.insert(logical, value);
                        }
                    }
                }
            } else {
                let (walk_root, walk_prefix) = loose_ref_list_walk_root(base, logical_prefix);
                if walk_root.is_dir() {
                    let mut storage_map = BTreeMap::new();
                    collect_loose_raw_refs(
                        &walk_root,
                        &walk_prefix,
                        logical_prefix,
                        &mut storage_map,
                    )?;
                    for (storage, value) in storage_map {
                        let logical = self.logical_name(&storage);
                        if logical_prefix.is_empty() || logical.starts_with(logical_prefix) {
                            map.insert(logical, value);
                        }
                    }
                }
            }
            if head_matches_prefix(logical_prefix) && base.join("HEAD").is_file() {
                if let Ok(r) = read_ref_file(&base.join("HEAD")) {
                    map.insert("HEAD".to_owned(), ref_to_raw(r));
                }
            }
        }
        Ok(map)
    }

    /// Reflogs a batch can append to: each update that carries a reflog, and the ref its
    /// symbolic chain (as recorded in `refs`) ends at. Updates without a reflog touch none,
    /// so plain deletes and CAS updates skip reading `logs/` entirely.
    fn load_reflogs_for_updates(
        &self,
        refs: &BTreeMap<String, RawRef>,
        updates: &[RefUpdate],
    ) -> Result<BTreeMap<String, Vec<ReflogEntry>>> {
        let mut names = BTreeSet::new();
        for update in updates.iter().filter(|u| u.reflog.is_some()) {
            let mut name = update.name.as_str();
            names.insert(name.to_owned());
            for _ in 0..crate::refs::SYMREF_MAXDEPTH {
                let Some(RawRef::Symbolic(next)) = refs.get(name) else {
                    break;
                };
                name = next;
                names.insert(name.to_owned());
            }
        }
        let mut out = BTreeMap::new();
        for name in names {
            let entries = read_reflog_at_path(&self.reflog_path(&name))?;
            if !entries.is_empty() {
                out.insert(name, entries);
            }
        }
        Ok(out)
    }

    fn insert_head_for_batch_state(&self, map: &mut BTreeMap<String, RawRef>) -> Result<()> {
        if map.contains_key("HEAD") {
            return Ok(());
        }
        for base in [self.common_dir.as_path(), self.git_dir.as_path()] {
            let head_path = base.join("HEAD");
            if head_path.is_file() {
                let raw = read_ref_file(&head_path).map(ref_to_raw)?;
                map.insert("HEAD".to_owned(), raw);
                break;
            }
        }
        Ok(())
    }

    fn loose_ref_file_exists(&self, refname: &str) -> Result<bool> {
        let route = self.route(refname);
        let path = route.storage_dir.join(&route.storage_name);
        Ok(path.is_file())
    }

    fn route(&self, refname: &str) -> RefStorageRoute {
        route_ref_storage_in_common(
            &self.git_dir,
            &self.common_dir,
            refname,
            self.namespace_prefix.as_deref(),
        )
    }

    fn logical_name(&self, storage: &str) -> String {
        crate::ref_namespace::logical_ref_name_with_prefix(
            self.namespace_prefix.as_deref(),
            storage,
        )
    }

    fn reflog_path(&self, logical: &str) -> PathBuf {
        let route = self.route(logical);
        route.storage_dir.join("logs").join(&route.storage_name)
    }

    fn ref_lock_paths(&self, refname: &str) -> (RefStorageRoute, PathBuf, PathBuf) {
        let route = self.route(refname);
        let path = route.storage_dir.join(&route.storage_name);
        let lock = lock_path_for_ref(&path);
        (route, path, lock)
    }

    fn needs_packed_lock(&self, updates: &[RefUpdate]) -> bool {
        updates.iter().any(|u| {
            if u.new_value.is_some() || u.flags.log_only {
                return false;
            }
            let storage = self.route(&u.name).storage_name;
            self.lookup_packed_oid(&storage).ok().flatten().is_some()
        })
    }

    fn acquire_ref_lock(lock: &Path, refname: &str, wait: LockWait) -> StoreResult<()> {
        use crate::lockfile::{LockError, LockFile};
        LockFile::acquire(lock, wait)
            .map(|guard| guard.detach())
            .map_err(|err| match err {
                LockError::Held { .. } => RefStoreError::LockHeld {
                    name: refname.to_owned(),
                },
                LockError::Io { source, .. } => RefStoreError::Corrupt(source.to_string()),
            })
    }

    fn acquire_ref_lock_mapped(
        &self,
        lock_path: &Path,
        refname: &str,
        wait: LockWait,
    ) -> Result<()> {
        use crate::lockfile::{LockError, LockFile};
        LockFile::acquire(lock_path, wait)
            .map(|guard| guard.detach())
            .map_err(|err| match err {
                LockError::Held { .. } => Error::RefStore(RefStoreError::LockHeld {
                    name: refname.to_owned(),
                }),
                LockError::Io { source, path } => map_git_dir_io(
                    &self.common_dir,
                    path.as_path(),
                    GitDirPathOperation::UpdateRef,
                    source,
                ),
            })
    }

    fn release_locks(batch: &PreparedFilesBatch) {
        for lock in batch.ref_locks.values() {
            let _ = fs::remove_file(lock);
        }
        if let Some(p) = &batch.packed_lock {
            let _ = fs::remove_file(p);
        }
    }

    fn write_loose_value(
        storage_dir: &Path,
        path: &Path,
        lock: &Path,
        value: &RawRef,
    ) -> Result<()> {
        remove_empty_ref_directory(path);
        if let Some(parent) = path.parent() {
            io_with_git_path(
                storage_dir,
                parent,
                GitDirPathOperation::CreateDir,
                fs::create_dir_all(parent),
            )?;
        }
        let content = match value {
            RawRef::Direct(oid) => format!("{oid}\n"),
            RawRef::Symbolic(target) => format!("ref: {target}\n"),
        };
        io_with_git_path(
            storage_dir,
            lock,
            GitDirPathOperation::UpdateRef,
            fs::write(lock, content),
        )?;
        io_with_git_path(
            storage_dir,
            path,
            GitDirPathOperation::UpdateRef,
            fs::rename(lock, path),
        )?;
        Ok(())
    }

    fn persist_case_only_loose_ref(
        &self,
        rename: &super::RefCaseOnlyRename,
        after: &RefBatchState,
        ctx: CommitContext<'_>,
    ) -> Result<()> {
        let Some(value) = after.refs.get(&rename.to) else {
            return Err(Error::Message(format!(
                "case-only rename missing destination ref '{}'",
                rename.to
            )));
        };
        let (_, from_path, from_lock) = self.ref_lock_paths(&rename.from);
        if let Some(parent) = from_lock.parent() {
            fs::create_dir_all(parent).map_err(Error::Io)?;
        }
        Self::acquire_ref_lock(&from_lock, &rename.from, ctx.ref_lock_wait).map_err(Error::from)?;
        if !from_path.is_file() {
            self.remove_packed_record(&rename.from, ctx)?;
            let route = self.route(&rename.from);
            FilesRefStore::write_loose_value(&route.storage_dir, &from_path, &from_lock, value)?;
        }
        let (_, to_path, _) = self.ref_lock_paths(&rename.to);
        if from_path != to_path {
            if let Some(parent) = to_path.parent() {
                fs::create_dir_all(parent).map_err(Error::Io)?;
            }
            fs::rename(&from_path, &to_path).map_err(Error::Io)?;
        }
        let _ = fs::remove_file(&from_lock);
        Ok(())
    }

    /// Drop `refname`'s `packed-refs` record unless the up-front batch rewrite already did.
    fn remove_packed_record(&self, refname: &str, ctx: CommitContext<'_>) -> Result<()> {
        let route = self.route(refname);
        if !ctx.lock_held {
            return remove_packed_ref(&route.storage_dir, &route.storage_name);
        }
        if ctx.pruned.contains(refname) {
            return Ok(());
        }
        remove_packed_ref_under_lock(&route.storage_dir, &route.storage_name)
    }

    /// Remove the `packed-refs` records of every ref `before` has and `after` drops, one
    /// rewrite per storage directory, ahead of any loose-file change (Git's order, so a
    /// deleted loose ref never uncovers a stale packed value).
    ///
    /// The returned [`PrunedPackedRecords`] covers every pruned name, the removed values the
    /// commit-time recheck still needs, and the records themselves for
    /// [`Self::restore_unprocessed_packed_records`].
    fn prune_deleted_packed_records(
        &self,
        before: &RefBatchState,
        after: &RefBatchState,
        updates: &[RefUpdate],
    ) -> Result<PrunedPackedRecords> {
        let checked: HashSet<&str> = updates
            .iter()
            .filter(|u| u.new_value.is_none() && !u.flags.log_only && u.expected != Expected::Any)
            .map(|u| u.name.as_str())
            .collect();
        let mut pruned = PrunedPackedRecords::default();
        let mut by_dir: BTreeMap<PathBuf, (HashSet<String>, HashMap<String, String>)> =
            BTreeMap::new();
        for name in before.refs.keys() {
            if after.refs.contains_key(name) {
                continue;
            }
            let route = self.route(name);
            let (storage_names, checked_names) = by_dir.entry(route.storage_dir).or_default();
            if checked.contains(name.as_str()) {
                checked_names.insert(route.storage_name.clone(), name.clone());
            }
            storage_names.insert(route.storage_name);
            pruned.names.insert(name.clone());
        }
        for (dir, (storage_names, checked_names)) in by_dir {
            let records = remove_packed_refs_under_lock(&dir, &storage_names)?;
            for record in &records {
                let Some(logical) = checked_names.get(packed_record_name(record)) else {
                    continue;
                };
                if let Some(oid) = packed_record_oid(record) {
                    pruned.removed.insert(logical.clone(), RawRef::Direct(oid));
                }
            }
            if !records.is_empty() {
                pruned.records.push((dir, records));
            }
        }
        self.invalidate_packed_cache_after_write();
        Ok(pruned)
    }

    /// After a commit failed part-way, write back the pruned `packed-refs` records of refs the
    /// batch never got to (names not in `persisted`), so a failed batch leaves them in place.
    fn restore_unprocessed_packed_records(
        &self,
        pruned: &PrunedPackedRecords,
        persisted: &HashSet<String>,
    ) -> Result<()> {
        let mut unprocessed: HashMap<PathBuf, HashSet<String>> = HashMap::new();
        for name in pruned.names.difference(persisted) {
            let route = self.route(name);
            unprocessed
                .entry(route.storage_dir)
                .or_default()
                .insert(route.storage_name);
        }
        for (dir, records) in &pruned.records {
            let Some(names) = unprocessed.get(dir) else {
                continue;
            };
            let restore: Vec<&str> = records
                .iter()
                .map(String::as_str)
                .filter(|record| names.contains(packed_record_name(record)))
                .collect();
            restore_packed_records_under_lock(dir, &restore)?;
        }
        self.invalidate_packed_cache_after_write();
        Ok(())
    }

    fn delete_loose_ref(&self, refname: &str, lock: &Path, ctx: CommitContext<'_>) -> Result<()> {
        let route = self.route(refname);
        let path = route.storage_dir.join(&route.storage_name);
        self.remove_packed_record(refname, ctx)?;
        remove_empty_ref_directory(&path);
        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(lock);
        if let Some(parent) = path.parent() {
            ctx.emptied_dirs
                .borrow_mut()
                .entry(parent.to_path_buf())
                .or_insert(route.storage_dir);
        }
        Ok(())
    }

    fn persist_reflog(&self, name: &str, entries: &[ReflogEntry]) -> Result<()> {
        let path = self.reflog_path(name);
        if entries.is_empty() {
            if let Some(parent) = path.parent() {
                io_with_git_path(
                    &self.common_dir,
                    parent,
                    GitDirPathOperation::CreateDir,
                    fs::create_dir_all(parent),
                )?;
            }
            io_with_git_path(
                &self.common_dir,
                &path,
                GitDirPathOperation::AppendReflog,
                fs::write(&path, ""),
            )?;
            return Ok(());
        }
        if let Some(parent) = path.parent() {
            io_with_git_path(
                &self.common_dir,
                parent,
                GitDirPathOperation::CreateDir,
                fs::create_dir_all(parent),
            )?;
        }
        let mut body = String::new();
        for entry in entries {
            let (old_hex, new_hex) =
                crate::refs::reflog_oid_hex_pair(&entry.old_oid, &entry.new_oid);
            if entry.message.is_empty() {
                body.push_str(&format!("{old_hex} {new_hex} {}\n", entry.identity));
            } else {
                body.push_str(&format!(
                    "{old_hex} {new_hex} {}\t{}\n",
                    entry.identity, entry.message
                ));
            }
        }
        io_with_git_path(
            &self.common_dir,
            &path,
            GitDirPathOperation::AppendReflog,
            fs::write(&path, body),
        )?;
        Ok(())
    }

    fn peel_ref_to_oid(&self, start: &str) -> Result<Option<ObjectId>> {
        let mut name = start.to_owned();
        for _ in 0..crate::refs::SYMREF_MAXDEPTH {
            match self.read_raw(&name)? {
                Some(RawRef::Direct(oid)) => return Ok(Some(oid)),
                Some(RawRef::Symbolic(next)) => name = next,
                None => return Ok(None),
            }
        }
        Err(Error::RefStore(RefStoreError::SymrefLoop))
    }

    fn recheck_update_at_commit(
        &self,
        update: &RefUpdate,
        batch_extras: &BTreeSet<String>,
        batch_start: &RefBatchState,
        state_before: &RefBatchState,
        ctx: CommitContext<'_>,
    ) -> StoreResult<()> {
        if update.flags.log_only {
            return Ok(());
        }
        let batch_differs =
            batch_start.refs.get(&update.name) != state_before.refs.get(&update.name);
        if !super::apply::expected_matches_for_update(state_before, update) {
            return Err(RefStoreError::ExpectedMismatch {
                name: update.name.clone(),
                expected: update.expected.clone(),
                actual: state_before.refs.get(&update.name).cloned(),
            });
        }
        if !batch_differs && update.new_value.is_none() && update.expected == Expected::Any {
            return Ok(());
        }
        let actual = if batch_differs {
            state_before.refs.get(&update.name).cloned()
        } else {
            self.read_raw(&update.name)
                .map_err(|e| RefStoreError::Corrupt(e.to_string()))?
                .or_else(|| ctx.removed.get(&update.name).cloned())
        };
        let actual_ref = actual.as_ref();
        if !batch_differs {
            let disk_ok = match &update.expected {
                Expected::Oid(want)
                    if matches!(actual_ref, Some(RawRef::Symbolic(_)))
                        && matches!(update.new_value.as_ref(), Some(RawRef::Direct(_)))
                        && !update.flags.no_deref =>
                {
                    self.peel_ref_to_oid(&update.name)
                        .map_err(|e| RefStoreError::Corrupt(e.to_string()))?
                        .is_some_and(|got| got == *want)
                }
                _ => expected_matches(actual_ref, &update.expected),
            };
            if !disk_ok {
                return Err(RefStoreError::ExpectedMismatch {
                    name: update.name.clone(),
                    expected: update.expected.clone(),
                    actual,
                });
            }
        }
        if update.new_value.is_some() && actual_ref.is_none() && !batch_differs {
            self.verify_create_available(&update.name, batch_extras)?;
        }
        Ok(())
    }

    /// Store `value` as `name` (`None` deletes it).
    fn persist_one_ref_change(
        &self,
        name: &str,
        value: Option<&RawRef>,
        lock: &Path,
        ctx: CommitContext<'_>,
    ) -> Result<()> {
        let route = self.route(name);
        let (_, path, _) = self.ref_lock_paths(name);
        match value {
            None => self.delete_loose_ref(name, lock, ctx),
            Some(value) => FilesRefStore::write_loose_value(&route.storage_dir, &path, lock, value),
        }
    }

    fn persist_one_ref_name(
        &self,
        name: &str,
        value: Option<&RawRef>,
        ctx: CommitContext<'_>,
        persisted_refs: &mut HashSet<String>,
    ) -> Result<()> {
        let (_, _, lock_path) = self.ref_lock_paths(name);
        if let Some(parent) = lock_path.parent() {
            io_with_git_path(
                &self.common_dir,
                parent,
                GitDirPathOperation::CreateDir,
                fs::create_dir_all(parent),
            )?;
        }
        self.acquire_ref_lock_mapped(&lock_path, name, ctx.ref_lock_wait)?;
        let write_result = self.persist_one_ref_change(name, value, &lock_path, ctx);
        let _ = fs::remove_file(&lock_path);
        write_result?;
        persisted_refs.insert(name.to_owned());
        Ok(())
    }

    /// Lock every ref `update` writes (and `update.name` when its expectation must be
    /// verified), recheck the expectation against disk under those locks, then write the refs.
    ///
    /// `running` is the batch state before `update`; `touched` is the one ref the update
    /// stores [`RefUpdate::new_value`] into (see [`ref_change_target`]).
    ///
    /// Holding the write targets' locks across the recheck matters for symbolic refs too: an
    /// update to `HEAD` that dereferences to `refs/heads/main` must exclude writers of `main`,
    /// not only of `HEAD`. Each successful write consumes its lock; anything left is released.
    #[allow(clippy::too_many_arguments)]
    fn persist_update_locked(
        &self,
        update: &RefUpdate,
        batch_extras: &BTreeSet<String>,
        before: &RefBatchState,
        running: &RefBatchState,
        touched: Option<&str>,
        ctx: CommitContext<'_>,
        persisted_refs: &mut HashSet<String>,
        writes_ref: bool,
        carried: &mut Option<(String, PathBuf)>,
    ) -> Result<()> {
        let value_after = |name: &str| {
            if touched == Some(name) {
                update.new_value.as_ref()
            } else {
                running.refs.get(name)
            }
        };
        let mut targets: Vec<String> = touched
            .filter(|t| *t != update.name && running.refs.get(*t) != update.new_value.as_ref())
            .map(str::to_owned)
            .into_iter()
            .collect();
        if writes_ref {
            targets.push(update.name.clone());
        }

        let mut held: BTreeMap<String, PathBuf> = BTreeMap::new();
        let release = |held: &BTreeMap<String, PathBuf>| {
            for lock in held.values() {
                let _ = fs::remove_file(lock);
            }
        };
        let verify_name = (!matches!(update.expected, Expected::Any)).then_some(&update.name);
        for name in targets.iter().chain(verify_name) {
            if held.contains_key(name) {
                continue;
            }
            if let Some((_, lock)) = carried.take_if(|(carried_name, _)| carried_name == name) {
                held.insert(name.clone(), lock);
                continue;
            }
            let (_, _, lock_path) = self.ref_lock_paths(name);
            let acquired = match lock_path.parent() {
                Some(parent) => io_with_git_path(
                    &self.common_dir,
                    parent,
                    GitDirPathOperation::CreateDir,
                    fs::create_dir_all(parent),
                ),
                None => Ok(()),
            }
            .and_then(|()| self.acquire_ref_lock_mapped(&lock_path, name, ctx.ref_lock_wait));
            if let Err(err) = acquired {
                release(&held);
                return Err(err);
            }
            held.insert(name.clone(), lock_path);
        }

        if let Err(err) = self.recheck_update_at_commit(update, batch_extras, before, running, ctx)
        {
            release(&held);
            return Err(err.into());
        }
        #[cfg(test)]
        tests::run_after_recheck_hook();

        for name in &targets {
            let Some(lock_path) = held.remove(name) else {
                continue;
            };
            if let Err(err) = self.persist_one_ref_change(name, value_after(name), &lock_path, ctx)
            {
                let _ = fs::remove_file(&lock_path);
                release(&held);
                return Err(err);
            }
            persisted_refs.insert(name.clone());
        }
        release(&held);
        Ok(())
    }

    fn persist_state_diff(
        &self,
        batch: &PreparedFilesBatch,
        carried: &mut Option<(String, PathBuf)>,
        emptied_dirs: &RefCell<BTreeMap<PathBuf, PathBuf>>,
    ) -> Result<()> {
        let before = &batch.before_commit;
        let after = &batch.after_commit;
        let updates = batch.updates.as_slice();
        let packed_lock_held = batch.packed_lock.is_some();
        let batch_extras: BTreeSet<String> = updates.iter().map(|u| u.name.clone()).collect();
        if updates.iter().any(|u| {
            u.new_value.is_some() && !u.flags.log_only && !before.refs.contains_key(&u.name)
        }) {
            self.forget_packed_cache();
        }

        let mut names: BTreeSet<String> = BTreeSet::new();
        names.extend(before.refs.keys().cloned());
        names.extend(after.refs.keys().cloned());
        names.extend(before.reflogs.keys().cloned());
        names.extend(after.reflogs.keys().cloned());

        for name in names {
            if before.reflogs.get(&name) != after.reflogs.get(&name) {
                let entries = after.reflogs.get(&name).cloned().unwrap_or_default();
                self.persist_reflog(&name, &entries)?;
            }
        }

        let pruned = if packed_lock_held {
            self.prune_deleted_packed_records(before, after, updates)?
        } else {
            PrunedPackedRecords::default()
        };
        let ctx = CommitContext {
            ref_lock_wait: batch.ref_lock_wait,
            lock_held: packed_lock_held,
            pruned: &pruned.names,
            removed: &pruned.removed,
            emptied_dirs,
        };

        let mut persisted_refs = HashSet::new();
        let persisted =
            self.persist_batch_refs(batch, &batch_extras, ctx, &mut persisted_refs, carried);
        if persisted.is_err() {
            // Keep the error that stopped the batch; a failed restore cannot be reported
            // alongside it.
            let _ = self.restore_unprocessed_packed_records(&pruned, &persisted_refs);
        }
        persisted?;
        self.invalidate_packed_cache_after_write();
        Ok(())
    }

    /// Write every ref change of `batch` (case-only renames, then each update under its locks,
    /// then anything the updates left out), recording each written name in `persisted_refs`.
    fn persist_batch_refs(
        &self,
        batch: &PreparedFilesBatch,
        batch_extras: &BTreeSet<String>,
        ctx: CommitContext<'_>,
        persisted_refs: &mut HashSet<String>,
        carried: &mut Option<(String, PathBuf)>,
    ) -> Result<()> {
        let before = &batch.before_commit;
        let after = &batch.after_commit;
        let updates = batch.updates.as_slice();
        for rename in &batch.case_only_renames {
            self.persist_case_only_loose_ref(rename, after, ctx)?;
            persisted_refs.insert(rename.from.clone());
            persisted_refs.insert(rename.to.clone());
        }
        let mut running = before.clone();
        for (idx, update) in updates.iter().enumerate() {
            if update.flags.log_only {
                apply_update(&mut running, update).map_err(Error::from)?;
                continue;
            }
            let touched = ref_change_target(&running, update).map_err(Error::from)?;
            let name = &update.name;
            let b = running.refs.get(name);
            let a = if touched.as_deref() == Some(name.as_str()) {
                update.new_value.as_ref()
            } else {
                b
            };
            let materialize_loose = update.new_value.is_some()
                && a.is_some()
                && b == a
                && !self
                    .loose_ref_file_exists(name)
                    .map_err(|e| RefStoreError::Corrupt(e.to_string()))?;
            let writes_ref = b != a || materialize_loose;

            self.persist_update_locked(
                update,
                batch_extras,
                before,
                &running,
                touched.as_deref(),
                ctx,
                persisted_refs,
                writes_ref,
                carried,
            )?;

            apply_update(&mut running, update).map_err(Error::from)?;
            crate::refs::test_ref_store_batch_pause_after_ref_write(idx, &self.common_dir);
        }

        for name in before
            .refs
            .keys()
            .chain(after.refs.keys())
            .collect::<BTreeSet<_>>()
        {
            if persisted_refs.contains(name) {
                continue;
            }
            let b = before.refs.get(name);
            let a = after.refs.get(name);
            if b == a {
                continue;
            }
            self.persist_one_ref_name(name, a, ctx, persisted_refs)?;
        }
        Ok(())
    }
}

impl FilesRefStore {
    /// [`RefStore::for_each_ref`], skipping the `packed-refs` records `keep` rejects.
    ///
    /// `keep` sees each packed record's storage name, object id and [`PackedPeel`] before
    /// anything is allocated for it. A loose ref is listed whatever `keep` says about a packed
    /// record of the same name.
    pub(crate) fn for_each_ref_filtered(
        &self,
        prefix: &str,
        keep: Option<PackedRecordFilter<'_>>,
        f: &mut dyn FnMut(&RefEntry) -> ControlFlow<()>,
    ) -> Result<()> {
        let prefix = prefix.trim_end_matches('/');
        let mut map = self.load_raw_map_prefix(prefix, keep)?;
        for (name, value) in map.split_off(prefix) {
            if !name.starts_with(prefix) {
                break;
            }
            let peeled = match &value {
                RawRef::Direct(_) => None,
                RawRef::Symbolic(target) => self.resolve(target).ok(),
            };
            let entry = RefEntry {
                name,
                value,
                peeled,
            };
            if f(&entry).is_break() {
                break;
            }
        }
        Ok(())
    }
}

impl RefStore for FilesRefStore {
    fn format(&self) -> RefStorageFormat {
        RefStorageFormat::Files
    }

    fn read_raw(&self, name: &str) -> Result<Option<RawRef>> {
        self.read_raw_at_route(&self.route(name))
    }

    fn for_each_ref(
        &self,
        prefix: &str,
        f: &mut dyn FnMut(&RefEntry) -> ControlFlow<()>,
    ) -> Result<()> {
        self.for_each_ref_filtered(prefix, None, f)
    }

    fn prepare(
        &self,
        txn: RefTransaction,
    ) -> StoreResult<Box<dyn PreparedRefTransaction + Send + '_>> {
        txn.validate()?;
        let (updates, reflog_renames, case_only_renames) = txn.into_parts();

        let mut prepared_guard = self.prepared.lock().unwrap_or_else(|e| e.into_inner());
        if prepared_guard.is_some() {
            return Err(RefStoreError::LockHeld {
                name: updates
                    .first()
                    .map(|u| u.name.clone())
                    .unwrap_or_else(|| "refs".to_owned()),
            });
        }

        let mut preliminary = self
            .load_batch_state_for_prepare(&updates, &reflog_renames, &case_only_renames)
            .map_err(|e| RefStoreError::Corrupt(e.to_string()))?;
        self.insert_create_conflict_neighbors(&mut preliminary.refs, &updates)
            .map_err(|e| RefStoreError::Corrupt(e.to_string()))?;
        verify_create_conflicts(&preliminary.refs, &updates)?;
        for rename in &reflog_renames {
            self.ensure_reflog_path_writable(&rename.to)?;
        }
        for rename in &case_only_renames {
            self.ensure_reflog_path_writable(&rename.to)?;
        }
        let lock_names = ref_lock_names_for_updates(&preliminary, &updates);
        let ref_lock_wait = LockWait::files_ref_lock_for_git_dir(&self.common_dir);

        let mut ref_locks: HashMap<String, PathBuf> = HashMap::new();
        for name in &lock_names {
            let (_, _, lock) = self.ref_lock_paths(name);
            if let Some(parent) = lock.parent() {
                fs::create_dir_all(parent).map_err(|e| RefStoreError::Corrupt(e.to_string()))?;
            }
            if let Err(err) = Self::acquire_ref_lock(&lock, name, ref_lock_wait) {
                for acquired in ref_locks.values() {
                    let _ = fs::remove_file(acquired);
                }
                return Err(err);
            }
            ref_locks.insert(name.clone(), lock);
        }

        if let Err(err) =
            simulate_batch_apply(&preliminary, &updates, &reflog_renames, &case_only_renames)
        {
            for acquired in ref_locks.values() {
                let _ = fs::remove_file(acquired);
            }
            return Err(err);
        }

        let extras: BTreeSet<String> = updates.iter().map(|u| u.name.clone()).collect();
        for update in &updates {
            if update.new_value.is_none() || update.flags.log_only {
                continue;
            }
            if preliminary.refs.contains_key(&update.name) {
                continue;
            }
            if let Err(reason) = self.verify_create_available(&update.name, &extras) {
                for acquired in ref_locks.values() {
                    let _ = fs::remove_file(acquired);
                }
                return Err(reason.into());
            }
        }

        let before_commit = preliminary;
        let mut after_commit = before_commit.clone();
        for update in &updates {
            apply_update(&mut after_commit, update)?;
        }
        for rename in &case_only_renames {
            apply_case_only_ref_rename(&mut after_commit, rename)?;
        }
        for rename in &reflog_renames {
            apply_reflog_rename(&mut after_commit, rename)?;
        }
        let mut changed = changed_ref_names(&before_commit.refs, &after_commit.refs);
        for update in &updates {
            if update.flags.log_only || update.new_value.is_none() {
                continue;
            }
            if before_commit.refs.contains_key(&update.name)
                && !self
                    .loose_ref_file_exists(&update.name)
                    .map_err(|e| RefStoreError::Corrupt(e.to_string()))?
            {
                changed.insert(update.name.clone());
            }
        }

        for name in &changed {
            if ref_locks.contains_key(name) {
                continue;
            }
            let (_, _, lock) = self.ref_lock_paths(name);
            if let Some(parent) = lock.parent() {
                fs::create_dir_all(parent).map_err(|e| RefStoreError::Corrupt(e.to_string()))?;
            }
            if let Err(err) = Self::acquire_ref_lock(&lock, name, ref_lock_wait) {
                for acquired in ref_locks.values() {
                    let _ = fs::remove_file(acquired);
                }
                return Err(err);
            }
            ref_locks.insert(name.clone(), lock);
        }

        let mut packed_lock = None;
        if self.needs_packed_lock(&updates) {
            let packed_path = super::paths::packed_refs_path(&self.common_dir);
            let lock = lock_path_for_ref(&packed_path);
            use crate::lockfile::{LockError, LockFile};
            let wait = LockWait::packed_refs_for_git_dir(&self.common_dir);
            if let Err(err) = LockFile::acquire(&lock, wait).map(|guard| guard.detach()) {
                for acquired in ref_locks.values() {
                    let _ = fs::remove_file(acquired);
                }
                return Err(match err {
                    LockError::Held { .. } => RefStoreError::LockHeld {
                        name: super::paths::PACKED_REFS.to_owned(),
                    },
                    LockError::Io { source, .. } => RefStoreError::Corrupt(source.to_string()),
                });
            }
            packed_lock = Some(lock);
        }

        *prepared_guard = Some(PreparedFilesBatch {
            updates,
            case_only_renames,
            ref_locks,
            packed_lock,
            ref_lock_wait,
            before_commit,
            after_commit,
        });

        Ok(Box::new(FilesPrepared {
            store: self,
            committed: false,
        }))
    }

    fn reflog_exists(&self, name: &str) -> Result<bool> {
        Ok(self.reflog_path(name).is_file())
    }

    fn for_each_reflog_entry(
        &self,
        name: &str,
        reverse: bool,
        f: &mut dyn FnMut(&ReflogEntry) -> ControlFlow<()>,
    ) -> Result<()> {
        let entries = read_reflog_at_path(&self.reflog_path(name))?;
        if reverse {
            for entry in entries.iter().rev() {
                if f(entry).is_break() {
                    break;
                }
            }
        } else {
            for entry in &entries {
                if f(entry).is_break() {
                    break;
                }
            }
        }
        Ok(())
    }

    fn create_reflog(&self, name: &str) -> Result<()> {
        validate_storable_refname(name).map_err(|e| Error::Message(e.to_string()))?;
        let path = self.reflog_path(name);
        if let Some(parent) = path.parent() {
            io_with_git_path(
                &self.common_dir,
                parent,
                GitDirPathOperation::CreateDir,
                fs::create_dir_all(parent),
            )?;
        }
        io_with_git_path(
            &self.common_dir,
            &path,
            GitDirPathOperation::AppendReflog,
            fs::OpenOptions::new().create(true).append(true).open(&path),
        )?;
        Ok(())
    }

    fn delete_reflog(&self, name: &str) -> Result<()> {
        let route = self.route(name);
        let logs_root = route.storage_dir.join("logs");
        let path = logs_root.join(&route.storage_name);
        let _ = fs::remove_file(&path);
        // Empty parents left under `logs/` would block a later ref whose reflog needs that
        // path as a file (deleting `a/b`, then creating `a`).
        let mut parent = path.parent();
        while let Some(dir) = parent.filter(|d| *d != logs_root && d.starts_with(&logs_root)) {
            if fs::remove_dir(dir).is_err() {
                break;
            }
            parent = dir.parent();
        }
        Ok(())
    }

    fn replace_reflog(&self, name: &str, entries: Vec<ReflogEntry>) -> Result<()> {
        self.persist_reflog(name, &entries)
    }

    fn for_each_reflog_ref(&self, f: &mut dyn FnMut(&str) -> ControlFlow<()>) -> Result<()> {
        let mut names = BTreeSet::new();
        for base in [self.common_dir.as_path(), self.git_dir.as_path()] {
            let logs = base.join("logs");
            if logs.is_dir() {
                collect_reflog_ref_names(&logs, "", self, &mut names)?;
            }
        }
        for name in names {
            if f(&name).is_break() {
                break;
            }
        }
        Ok(())
    }

    fn as_files(&self) -> Option<&FilesRefStore> {
        Some(self)
    }
}

struct FilesPrepared<'a> {
    store: &'a FilesRefStore,
    committed: bool,
}

impl PreparedRefTransaction for FilesPrepared<'_> {
    fn commit(mut self: Box<Self>) -> StoreResult<()> {
        let mut guard = self
            .store
            .prepared
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let Some(mut batch) = guard.take() else {
            return Ok(());
        };

        // The first update is written before any other ref, so it keeps the lock it took in
        // `prepare` instead of releasing and re-creating it. The rest are released here and
        // re-taken one update at a time, so other writers can touch later refs mid-batch.
        let mut ref_locks = std::mem::take(&mut batch.ref_locks);
        let mut carried = batch
            .updates
            .iter()
            .find(|u| !u.flags.log_only)
            .and_then(|u| ref_locks.remove_entry(&u.name));
        for lock in ref_locks.values() {
            let _ = fs::remove_file(lock);
        }
        // The packed cache revalidates against the file's size and mtime on every read, so
        // rechecks under the ref locks see concurrent rewrites without a forced re-parse.
        let emptied_dirs = RefCell::new(BTreeMap::new());
        let persisted = self
            .store
            .persist_state_diff(&batch, &mut carried, &emptied_dirs);
        if let Some((_, lock)) = carried {
            let _ = fs::remove_file(lock);
        }
        // Pruning runs after every lock of the batch is gone: a leftover lock file would make
        // its directory look non-empty (issue #1317). Deepest first, so a parent is only
        // checked once its emptied children are removed.
        let mut emptied_dirs: Vec<_> = emptied_dirs.into_inner().into_iter().collect();
        emptied_dirs.sort_by_key(|(dir, _)| std::cmp::Reverse(dir.components().count()));
        for (dir, storage_dir) in &emptied_dirs {
            prune_empty_ref_dirs(storage_dir, dir);
        }
        self.store.invalidate_packed_cache_after_write();
        if let Err(err) = persisted {
            if let Some(p) = &batch.packed_lock {
                let _ = fs::remove_file(p);
            }
            return Err(match err {
                Error::RefStore(store_err) => store_err,
                other => RefStoreError::Corrupt(other.to_string()),
            });
        }
        if let Some(p) = &batch.packed_lock {
            let _ = fs::remove_file(p);
        }
        self.committed = true;
        Ok(())
    }

    fn abort(mut self: Box<Self>) -> StoreResult<()> {
        let mut guard = self
            .store
            .prepared
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(batch) = guard.take() {
            FilesRefStore::release_locks(&batch);
        }
        self.store.invalidate_packed_cache_after_write();
        self.committed = true;
        Ok(())
    }
}

impl Drop for FilesPrepared<'_> {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        if let Ok(mut guard) = self.store.prepared.lock() {
            if let Some(batch) = guard.take() {
                FilesRefStore::release_locks(&batch);
            }
        }
        self.store.invalidate_packed_cache_after_write();
    }
}

fn head_matches_prefix(logical_prefix: &str) -> bool {
    let prefix = logical_prefix.trim_end_matches('/');
    if prefix.is_empty() {
        // `git for-each-ref` with no pattern lists refs under `refs/`, not `HEAD`.
        return false;
    }
    "HEAD".starts_with(prefix) || prefix.starts_with("HEAD")
}

fn ref_to_raw(r: Ref) -> RawRef {
    match r {
        Ref::Direct(oid) => RawRef::Direct(oid),
        Ref::Symbolic(s) => RawRef::Symbolic(s),
    }
}

/// Read every loose ref file below `dir` into `out`, keyed by `prefix` plus its relative path.
///
/// Only names starting with `want` are read; entries that cannot lead to one are skipped
/// without a `stat`, so a partial-name prefix such as `refs/tags/v1` does not read every
/// file in `refs/tags/`. An empty `want` reads everything.
fn collect_loose_raw_refs(
    dir: &Path,
    prefix: &str,
    want: &str,
    out: &mut BTreeMap<String, RawRef>,
) -> Result<()> {
    let read = match fs::read_dir(dir) {
        Ok(r) => r,
        Err(e)
            if e.kind() == io::ErrorKind::NotFound || e.kind() == io::ErrorKind::NotADirectory =>
        {
            return Ok(());
        }
        Err(e) => return Err(Error::Io(e)),
    };
    for entry in read {
        let entry = entry?;
        let name_owned = entry.file_name().into_string().map_err(|_| {
            Error::Io(io::Error::new(
                io::ErrorKind::InvalidData,
                "non-utf8 ref path component",
            ))
        })?;
        let refname = format!("{prefix}{name_owned}");
        let wanted = refname.starts_with(want);
        let leads_to_wanted = || {
            want.strip_prefix(refname.as_str())
                .is_some_and(|rest| rest.starts_with('/'))
        };
        if !wanted && !leads_to_wanted() {
            continue;
        }
        let path = entry.path();
        let meta = fs::metadata(&path).map_err(Error::Io)?;
        if meta.is_dir() {
            collect_loose_raw_refs(&path, &format!("{refname}/"), want, out)?;
        } else if meta.is_file() && wanted {
            if let Ok(r) = read_ref_file(&path) {
                out.insert(refname, ref_to_raw(r));
            }
        }
    }
    Ok(())
}

/// Add `<prefix>/<relative path>` for every file below `dir` to `out`; a missing or unreadable
/// directory adds nothing.
fn collect_loose_file_names(dir: &Path, prefix: &str, out: &mut BTreeSet<String>) {
    let Ok(read) = fs::read_dir(dir) else {
        return;
    };
    for entry in read.flatten() {
        let name = format!("{prefix}/{}", entry.file_name().to_string_lossy());
        let path = entry.path();
        match fs::metadata(&path) {
            Ok(meta) if meta.is_dir() => collect_loose_file_names(&path, &name, out),
            Ok(meta) if meta.is_file() => {
                out.insert(name);
            }
            _ => {}
        }
    }
}

#[cfg(unix)]
fn file_identity(meta: &fs::Metadata) -> Option<(u64, u64, i64, i64)> {
    use std::os::unix::fs::MetadataExt;
    Some((meta.dev(), meta.ino(), meta.ctime(), meta.ctime_nsec()))
}

#[cfg(not(unix))]
fn file_identity(_meta: &fs::Metadata) -> Option<(u64, u64, i64, i64)> {
    None
}

fn read_reflog_at_path(path: &Path) -> Result<Vec<ReflogEntry>> {
    let content = match fs::read_to_string(path) {
        Ok(c) => c,
        // A ref being created can collide with a file or directory in `logs/`; that is a
        // missing log here, and the ref conflict check reports the collision.
        Err(e)
            if matches!(
                e.kind(),
                io::ErrorKind::NotFound
                    | io::ErrorKind::NotADirectory
                    | io::ErrorKind::IsADirectory
            ) =>
        {
            return Ok(Vec::new());
        }
        Err(e) => return Err(Error::Io(e)),
    };
    Ok(content
        .lines()
        .filter_map(reflog::parse_reflog_line_for_store)
        .collect())
}

fn changed_ref_names(
    before: &BTreeMap<String, RawRef>,
    after: &BTreeMap<String, RawRef>,
) -> BTreeSet<String> {
    let mut names: BTreeSet<String> = BTreeSet::new();
    names.extend(before.keys().cloned());
    names.extend(after.keys().cloned());
    names
        .into_iter()
        .filter(|name| before.get(name) != after.get(name))
        .collect()
}

fn collect_reflog_ref_names(
    dir: &Path,
    prefix: &str,
    store: &FilesRefStore,
    out: &mut BTreeSet<String>,
) -> Result<()> {
    let read = match fs::read_dir(dir) {
        Ok(r) => r,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(Error::Io(e)),
    };
    for entry in read {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let refname = format!("{prefix}{name}");
        let path = entry.path();
        if path.is_dir() {
            collect_reflog_ref_names(&path, &format!("{refname}/"), store, out)?;
        } else if path.is_file() {
            out.insert(store.logical_name(&refname));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;
    use crate::refs::store::RefUpdateFlags;

    // hygiene: test-only commit interleaving hook
    thread_local! {
        static AFTER_RECHECK: RefCell<Option<Box<dyn FnOnce()>>> = const { RefCell::new(None) };
    }

    /// Runs (once) the hook installed by [`after_next_recheck`] on this thread.
    pub(super) fn run_after_recheck_hook() {
        if let Some(hook) = AFTER_RECHECK.with(|slot| slot.borrow_mut().take()) {
            hook();
        }
    }

    /// Run `hook` inside the next commit on this thread, right after an update passed its
    /// on-disk compare-and-swap recheck and before the ref is written.
    fn after_next_recheck(hook: impl FnOnce() + 'static) {
        AFTER_RECHECK.with(|slot| *slot.borrow_mut() = Some(Box::new(hook)));
    }

    fn oid(byte: u8) -> ObjectId {
        let mut bytes = [0u8; 20];
        bytes[19] = byte;
        ObjectId::from_bytes(&bytes).expect("oid")
    }

    fn store(git_dir: &Path) -> FilesRefStore {
        FilesRefStore::open(FilesRefStoreConfig {
            git_dir: git_dir.to_path_buf(),
            common_dir: git_dir.to_path_buf(),
            namespace_prefix: None,
        })
    }

    fn set(
        store: &FilesRefStore,
        name: &str,
        value: ObjectId,
        expected: Expected,
    ) -> StoreResult<()> {
        let txn = RefTransaction::new().update(RefUpdate {
            name: name.to_owned(),
            new_value: Some(RawRef::Direct(value)),
            expected,
            reflog: None,
            flags: RefUpdateFlags::default(),
        })?;
        store.prepare(txn)?.commit()
    }

    fn git_dir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::create_dir_all(dir.path().join("refs/heads")).expect("refs");
        fs::write(dir.path().join("HEAD"), "ref: refs/heads/main\n").expect("HEAD");
        dir
    }

    fn listed_names(store: &FilesRefStore, prefix: &str) -> Vec<String> {
        let mut names = Vec::new();
        store
            .for_each_ref(prefix, &mut |entry| {
                names.push(entry.name.clone());
                ControlFlow::Continue(())
            })
            .expect("list");
        names
    }

    #[test]
    fn partial_name_prefix_lists_loose_refs_without_visiting_siblings() {
        let dir = git_dir();
        let tags = dir.path().join("refs/tags");
        for name in ["v1", "v10", "v1x/a", "v2/deep", "w"] {
            let path = tags.join(name);
            fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
            fs::write(path, format!("{}\n", oid(1))).expect("ref");
        }
        // A sibling that cannot be stat'ed fails any walk that looks at it.
        #[cfg(unix)]
        std::os::unix::fs::symlink(tags.join("missing-target"), tags.join("zzz"))
            .expect("dangling symlink");
        let store = store(dir.path());

        assert_eq!(
            listed_names(&store, "refs/tags/v1"),
            ["refs/tags/v1", "refs/tags/v10", "refs/tags/v1x/a"]
        );
        assert_eq!(listed_names(&store, "refs/tags/v2/"), ["refs/tags/v2/deep"]);
        assert_eq!(listed_names(&store, "refs/tags/w"), ["refs/tags/w"]);
        assert!(listed_names(&store, "refs/tags/x").is_empty());
    }

    /// Two writers that both expect `expected` must not both succeed: the one that commits
    /// first holds the ref lock from its recheck to its write.
    fn assert_racing_writer_is_locked_out(expected: Expected, seed: Option<ObjectId>) {
        let dir = git_dir();
        let name = "refs/notes/commits";
        let writer = store(dir.path());
        if let Some(seed) = seed {
            set(&writer, name, seed, Expected::Missing).expect("seed");
        }

        let other = store(dir.path());
        let racing_expected = expected.clone();
        let racer = std::rc::Rc::new(RefCell::new(None));
        let racer_slot = racer.clone();
        after_next_recheck(move || {
            *racer_slot.borrow_mut() = Some(set(&other, name, oid(2), racing_expected));
        });
        set(&writer, name, oid(3), expected).expect("first committer wins");

        let racer = racer.borrow_mut().take().expect("hook ran");
        assert!(
            matches!(racer, Err(RefStoreError::LockHeld { .. })),
            "racing writer must not commit while the ref is locked: {racer:?}"
        );
        assert_eq!(
            writer.read_raw(name).expect("read"),
            Some(RawRef::Direct(oid(3)))
        );
        assert!(
            !dir.path().join("refs/notes/commits.lock").exists(),
            "the lock is released after the write"
        );
    }

    #[test]
    fn expected_oid_update_holds_the_lock_through_the_write() {
        assert_racing_writer_is_locked_out(Expected::Oid(oid(1)), Some(oid(1)));
    }

    #[test]
    fn expected_missing_create_holds_the_lock_through_the_write() {
        assert_racing_writer_is_locked_out(Expected::Missing, None);
    }

    #[test]
    fn symbolic_update_holds_the_dereferenced_target_lock_through_the_write() {
        let dir = git_dir();
        let writer = store(dir.path());
        set(&writer, "refs/heads/main", oid(1), Expected::Missing).expect("seed main");

        let other = store(dir.path());
        let racer = std::rc::Rc::new(RefCell::new(None));
        let racer_slot = racer.clone();
        after_next_recheck(move || {
            *racer_slot.borrow_mut() = Some(set(
                &other,
                "refs/heads/main",
                oid(2),
                Expected::Oid(oid(1)),
            ));
        });
        set(&writer, "HEAD", oid(3), Expected::Oid(oid(1))).expect("update through HEAD");

        let racer = racer.borrow_mut().take().expect("hook ran");
        assert!(
            matches!(racer, Err(RefStoreError::LockHeld { .. })),
            "a direct writer of the HEAD target must be locked out: {racer:?}"
        );
        assert_eq!(
            writer.read_raw("refs/heads/main").expect("read"),
            Some(RawRef::Direct(oid(3)))
        );
        assert_eq!(
            writer.read_raw("HEAD").expect("read"),
            Some(RawRef::Symbolic("refs/heads/main".to_owned()))
        );
        for lock in ["HEAD.lock", "refs/heads/main.lock"] {
            assert!(!dir.path().join(lock).exists(), "{lock} released");
        }
    }

    #[test]
    fn create_availability_matches_the_path_based_check() {
        let dir = git_dir();
        let path = dir.path();
        let hex = oid(1).to_hex();
        fs::write(
            path.join("packed-refs"),
            format!(
                "# pack-refs with: peeled fully-peeled sorted \n\
                 {hex} refs/heads/p\n\
                 {hex} refs/heads/pd/x\n\
                 {hex} refs/heads/pd-sibling\n\
                 {hex} refs/tags/v1\n"
            ),
        )
        .expect("packed-refs");
        for loose in ["refs/heads/l", "refs/heads/ld/y/z"] {
            let file = path.join(loose);
            fs::create_dir_all(file.parent().expect("parent")).expect("dirs");
            fs::write(file, format!("{hex}\n")).expect("loose");
        }
        let refs = store(path);
        let extras: BTreeSet<String> = ["refs/heads/batch/one".to_owned()].into();
        for name in [
            "refs/heads/new",
            "refs/heads/p/child",
            "refs/heads/pd",
            "refs/heads/pd-sibling/x",
            "refs/heads/l/child",
            "refs/heads/ld",
            "refs/heads/ld/y",
            "refs/heads/batch",
            "refs/heads/batch/one/two",
            "refs/tags/v1/x",
            "refs/tags/v2",
        ] {
            assert_eq!(
                refs.verify_create_available(name, &extras),
                crate::refs::verify_refname_available_for_create(
                    path,
                    name,
                    &extras,
                    &HashSet::new()
                ),
                "{name}"
            );
        }
    }

    #[test]
    fn mapped_packed_refs_are_reread_after_replacement() {
        let dir = git_dir();
        let path = dir.path();
        let records = |value: ObjectId| -> Vec<crate::refs::PackedRefRecord> {
            (0..500)
                .map(|i| crate::refs::PackedRefRecord {
                    refname: format!("refs/tags/t{i:04}"),
                    oid: value,
                    peeled: None,
                })
                .collect()
        };
        crate::refs::write_packed_refs_file(path, &records(oid(1))).expect("pack");
        assert!(fs::metadata(path.join("packed-refs")).expect("meta").len() > 8192);
        let refs = store(path);
        assert_eq!(
            refs.read_raw("refs/tags/t0499").expect("read"),
            Some(RawRef::Direct(oid(1)))
        );
        let mut replaced = records(oid(2));
        replaced.truncate(10);
        crate::refs::write_packed_refs_file(path, &replaced).expect("repack");
        assert_eq!(refs.read_raw("refs/tags/t0499").expect("read"), None);
        assert_eq!(
            refs.read_raw("refs/tags/t0009").expect("read"),
            Some(RawRef::Direct(oid(2)))
        );
    }

    /// Replace `packed-refs` by renaming a same-length file over it, then put back the old
    /// mtime (as a coarse-mtime filesystem would show it).
    fn replace_packed_refs_keeping_len_and_mtime(path: &Path, body: &str) {
        let packed = path.join("packed-refs");
        let before = fs::metadata(&packed).expect("meta");
        assert_eq!(before.len(), body.len() as u64, "same length");
        let tmp = path.join("packed-refs.new");
        fs::write(&tmp, body).expect("write");
        fs::File::options()
            .write(true)
            .open(&tmp)
            .expect("open")
            .set_modified(before.modified().expect("mtime"))
            .expect("pin mtime");
        fs::rename(&tmp, &packed).expect("rename");
        let after = fs::metadata(&packed).expect("meta");
        assert_eq!(after.len(), before.len());
        assert_eq!(after.modified().ok(), before.modified().ok());
    }

    #[test]
    fn create_rechecks_a_packed_refs_replaced_with_same_len_and_mtime() {
        assert_create_blocked_by_replaced_packed_descendant(false);
    }

    #[test]
    fn create_rechecks_packed_refs_even_when_the_stamp_cannot_tell() {
        assert_create_blocked_by_replaced_packed_descendant(true);
    }

    /// Prime a store with `refs/heads/blocked-child` packed, swap in an equal-length
    /// `packed-refs` holding `refs/heads/blocked/child`, then create `refs/heads/blocked`.
    fn assert_create_blocked_by_replaced_packed_descendant(stamp_collides: bool) {
        let dir = git_dir();
        let path = dir.path();
        let hex = oid(1).to_hex();
        let header = "# pack-refs with: peeled fully-peeled sorted \n";
        fs::write(
            path.join("packed-refs"),
            format!("{header}{hex} refs/heads/blocked-child\n"),
        )
        .expect("packed-refs");
        let writer = store(path);
        assert_eq!(
            writer.read_raw("refs/heads/blocked-child").expect("prime"),
            Some(RawRef::Direct(oid(1)))
        );
        replace_packed_refs_keeping_len_and_mtime(
            path,
            &format!("{header}{hex} refs/heads/blocked/child\n"),
        );
        if stamp_collides {
            // A replacement no stamp can see (coarse timestamps and a reused inode): only a
            // fresh read under the ref lock notices it.
            let mut guard = writer.packed_cache.lock().expect("cache");
            guard.stamp = FilesRefStore::packed_refs_stamp(&path.join("packed-refs"));
        }

        let err = set(&writer, "refs/heads/blocked", oid(2), Expected::Missing)
            .expect_err("packed descendant blocks the create");
        assert!(
            matches!(
                err,
                RefStoreError::NameUnavailable {
                    reason: RefnameUnavailable::DescendantExists { ref blocking, .. }
                } if blocking == "refs/heads/blocked/child"
            ),
            "{err:?}"
        );
        assert!(!path.join("refs/heads/blocked").exists());
        assert!(!path.join("refs/heads/blocked.lock").exists());
    }

    #[test]
    fn create_with_reflog_next_to_a_ref_file_in_logs_reports_the_conflict() {
        let dir = git_dir();
        let path = dir.path();
        let writer = store(path);
        set(&writer, "refs/heads/a", oid(1), Expected::Missing).expect("create");
        fs::create_dir_all(path.join("logs/refs/heads")).expect("logs");
        fs::write(path.join("logs/refs/heads/a"), "").expect("log");
        let txn = RefTransaction::new()
            .update(RefUpdate {
                name: "refs/heads/a/b".to_owned(),
                new_value: Some(RawRef::Direct(oid(2))),
                expected: Expected::Missing,
                reflog: Some(super::super::ReflogUpdate {
                    identity: "T <t@t>".into(),
                    message: "create".into(),
                    time: time::OffsetDateTime::UNIX_EPOCH,
                    old_oid: None,
                    new_oid: None,
                }),
                flags: RefUpdateFlags::default(),
            })
            .expect("txn");
        let err = writer.prepare(txn).map(|_| ()).expect_err("conflict");
        assert!(
            matches!(
                err,
                RefStoreError::NameUnavailable {
                    reason: RefnameUnavailable::AncestorExists { ref blocking, .. }
                } if blocking == "refs/heads/a"
            ),
            "{err:?}"
        );
        assert!(!path.join("refs/heads/a.lock").exists());
    }

    #[test]
    fn failed_recheck_releases_the_lock() {
        let dir = git_dir();
        let name = "refs/heads/main";
        let writer = store(dir.path());
        set(&writer, name, oid(1), Expected::Missing).expect("create");
        let other = store(dir.path());
        set(&other, name, oid(2), Expected::Oid(oid(1))).expect("other moves it");

        let err = set(&writer, name, oid(3), Expected::Oid(oid(1))).expect_err("stale");
        assert!(
            matches!(err, RefStoreError::ExpectedMismatch { .. }),
            "{err:?}"
        );
        assert!(!dir.path().join("refs/heads/main.lock").exists());
        set(&writer, name, oid(3), Expected::Oid(oid(2))).expect("retry against current value");
        assert_eq!(
            writer.read_raw(name).expect("read"),
            Some(RawRef::Direct(oid(3)))
        );
    }

    fn delete(name: &str, expected: Expected) -> RefUpdate {
        RefUpdate {
            name: name.to_owned(),
            new_value: None,
            expected,
            reflog: None,
            flags: RefUpdateFlags::default(),
        }
    }

    fn commit_all(store: &FilesRefStore, updates: Vec<RefUpdate>) -> StoreResult<()> {
        store
            .prepare(RefTransaction::from_updates(updates)?)?
            .commit()
    }

    /// `packed-refs` with twenty `refs/remotes/o/x<i>`, a peeled tag and `main`, plus a loose
    /// `x5` that overrides its packed value.
    fn packed_remote_refs(path: &Path) {
        let mut body = String::from("# pack-refs with: peeled fully-peeled sorted \n");
        body.push_str(&format!("{} refs/heads/main\n", oid(4)));
        let mut remotes: Vec<String> = (0..20).map(|i| format!("refs/remotes/o/x{i}")).collect();
        remotes.sort();
        for name in &remotes {
            body.push_str(&format!("{} {name}\n", oid(1)));
        }
        body.push_str(&format!("{} refs/tags/t\n^{}\n", oid(2), oid(3)));
        fs::write(path.join("packed-refs"), body).expect("packed-refs");
        fs::create_dir_all(path.join("refs/remotes/o")).expect("dir");
        fs::write(path.join("refs/remotes/o/x5"), format!("{}\n", oid(9))).expect("loose");
    }

    #[test]
    fn batch_delete_of_packed_refs_rewrites_packed_refs_once_and_keeps_the_rest() {
        let dir = git_dir();
        let path = dir.path();
        packed_remote_refs(path);
        let writer = store(path);
        let updates = (0..20)
            .map(|i| {
                let expected = match i {
                    0 => Expected::Oid(oid(1)),
                    5 => Expected::Oid(oid(9)),
                    7 => Expected::Exists,
                    _ => Expected::Any,
                };
                delete(&format!("refs/remotes/o/x{i}"), expected)
            })
            .collect();
        commit_all(&writer, updates).expect("batch delete");

        for i in 0..20 {
            let name = format!("refs/remotes/o/x{i}");
            assert_eq!(writer.read_raw(&name).expect("read"), None, "{name}");
        }
        let packed = fs::read_to_string(path.join("packed-refs")).expect("packed");
        assert_eq!(
            packed,
            format!(
                "# pack-refs with: peeled fully-peeled sorted\n{} refs/heads/main\n{} refs/tags/t\n^{}\n",
                oid(4),
                oid(2),
                oid(3)
            )
        );
        assert!(
            !path.join("refs/remotes").exists(),
            "emptied dirs are pruned"
        );
        assert!(!path.join("packed-refs.lock").exists());
        assert!(path.join("refs/heads").is_dir());
    }

    #[test]
    fn stale_expectation_on_packed_ref_rejects_the_whole_batch() {
        let dir = git_dir();
        let path = dir.path();
        packed_remote_refs(path);
        let before = fs::read_to_string(path.join("packed-refs")).expect("packed");
        let writer = store(path);
        let err = commit_all(
            &writer,
            vec![
                delete("refs/remotes/o/x1", Expected::Any),
                delete("refs/remotes/o/x2", Expected::Oid(oid(7))),
            ],
        )
        .expect_err("stale");
        assert!(
            matches!(err, RefStoreError::ExpectedMismatch { ref name, .. } if name == "refs/remotes/o/x2"),
            "{err:?}"
        );
        assert_eq!(
            fs::read_to_string(path.join("packed-refs")).expect("packed"),
            before
        );
        assert!(!path.join("packed-refs.lock").exists());
        assert!(!path.join("refs/remotes/o/x1.lock").exists());
    }

    #[test]
    fn batch_delete_prunes_nested_empty_dirs_deepest_first() {
        let dir = git_dir();
        let path = dir.path();
        let writer = store(path);
        for name in ["refs/heads/x/y/z", "refs/heads/x/w", "refs/heads/keep/k"] {
            set(&writer, name, oid(1), Expected::Missing).expect("create");
        }
        commit_all(
            &writer,
            vec![
                delete("refs/heads/x/w", Expected::Oid(oid(1))),
                delete("refs/heads/x/y/z", Expected::Any),
            ],
        )
        .expect("delete");
        assert!(!path.join("refs/heads/x").exists());
        assert!(path.join("refs/heads/keep/k").is_file());
    }

    #[test]
    fn deleting_through_a_symref_removes_the_packed_target() {
        let dir = git_dir();
        let path = dir.path();
        packed_remote_refs(path);
        fs::write(path.join("refs/remotes/o/HEAD"), "ref: refs/remotes/o/x3\n").expect("symref");
        let writer = store(path);
        commit_all(&writer, vec![delete("refs/remotes/o/HEAD", Expected::Any)])
            .expect("deref delete");
        assert_eq!(writer.read_raw("refs/remotes/o/x3").expect("read"), None);
        assert_eq!(
            writer.read_raw("refs/remotes/o/HEAD").expect("read"),
            Some(RawRef::Symbolic("refs/remotes/o/x3".to_owned()))
        );
        let packed = fs::read_to_string(path.join("packed-refs")).expect("packed");
        assert!(!packed.contains("refs/remotes/o/x3\n"), "{packed}");
        assert!(packed.contains("refs/remotes/o/x4\n"), "{packed}");
    }

    #[test]
    fn late_commit_time_mismatch_keeps_packed_refs_the_batch_never_reached() {
        let dir = git_dir();
        let path = dir.path();
        packed_remote_refs(path);
        let writer = store(path);
        let other = store(path);
        after_next_recheck(move || {
            set(&other, "refs/remotes/o/x9", oid(7), Expected::Any).expect("concurrent write");
        });
        let updates = (0..20)
            .map(|i| {
                let expected = if i == 9 {
                    Expected::Oid(oid(1))
                } else {
                    Expected::Any
                };
                delete(&format!("refs/remotes/o/x{i}"), expected)
            })
            .collect();
        let err = commit_all(&writer, updates).expect_err("x9 moved under the batch");
        assert!(
            matches!(err, RefStoreError::ExpectedMismatch { ref name, .. } if name == "refs/remotes/o/x9"),
            "{err:?}"
        );

        for i in 0..9 {
            let name = format!("refs/remotes/o/x{i}");
            assert_eq!(
                writer.read_raw(&name).expect("read"),
                None,
                "{name} was deleted"
            );
        }
        assert_eq!(
            writer.read_raw("refs/remotes/o/x9").expect("read"),
            Some(RawRef::Direct(oid(7))),
            "the concurrent value survives"
        );
        for i in 10..20 {
            let name = format!("refs/remotes/o/x{i}");
            assert_eq!(
                writer.read_raw(&name).expect("read"),
                Some(RawRef::Direct(oid(1))),
                "{name} was never processed"
            );
        }
        let packed = fs::read_to_string(path.join("packed-refs")).expect("packed");
        let mut expected = format!(
            "# pack-refs with: peeled fully-peeled sorted\n{} refs/heads/main\n",
            oid(4)
        );
        let mut kept: Vec<String> = (9..20).map(|i| format!("refs/remotes/o/x{i}")).collect();
        kept.sort();
        for name in kept {
            expected.push_str(&format!("{} {name}\n", oid(1)));
        }
        expected.push_str(&format!("{} refs/tags/t\n^{}\n", oid(2), oid(3)));
        assert_eq!(packed, expected);
        assert!(!path.join("packed-refs.lock").exists());
        assert!(!path.join("refs/remotes/o/x10.lock").exists());
    }

    #[test]
    fn restore_merges_records_back_in_sorted_order_with_peel_lines() {
        let dir = git_dir();
        let path = dir.path();
        fs::write(
            path.join("packed-refs"),
            format!(
                "# pack-refs with: peeled fully-peeled sorted \n{} refs/heads/b\n{} refs/tags/z\n^{}\n",
                oid(1),
                oid(2),
                oid(3)
            ),
        )
        .expect("packed");
        let names: HashSet<String> = ["refs/heads/b", "refs/tags/z"]
            .into_iter()
            .map(str::to_owned)
            .collect();
        let before = fs::read_to_string(path.join("packed-refs")).expect("packed");
        let removed = remove_packed_refs_under_lock(path, &names).expect("remove");
        assert_eq!(removed.len(), 2);
        assert_eq!(packed_record_name(&removed[1]), "refs/tags/z");
        assert_eq!(packed_record_oid(&removed[1]), Some(oid(2)));
        fs::write(
            path.join("packed-refs"),
            format!(
                "# pack-refs with: peeled fully-peeled sorted\n{} refs/heads/a\n{} refs/heads/c\n",
                oid(5),
                oid(6)
            ),
        )
        .expect("packed");
        let records: Vec<&str> = removed.iter().map(String::as_str).collect();
        restore_packed_records_under_lock(path, &records).expect("restore");
        assert_eq!(
            fs::read_to_string(path.join("packed-refs")).expect("packed"),
            format!(
                "# pack-refs with: peeled fully-peeled sorted\n{} refs/heads/a\n{} refs/heads/b\n{} refs/heads/c\n{} refs/tags/z\n^{}\n",
                oid(5),
                oid(1),
                oid(6),
                oid(2),
                oid(3)
            )
        );
        assert!(before.contains("refs/tags/z"));
    }
}
