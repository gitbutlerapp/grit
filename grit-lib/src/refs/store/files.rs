//! Filesystem ref backend (loose refs + `packed-refs`) implementing [`super::RefStore`].

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs;
use std::io;
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::error::{Error, Result};
use crate::objects::ObjectId;
use crate::reflog::{self, ReflogEntry};
use crate::refs::{
    lock_path_for_ref, loose_ref_list_walk_root, prune_empty_loose_ref_parents,
    read_packed_refs_file_view, read_packed_refs_map_for_prefix, read_ref_file,
    remove_empty_ref_directory, remove_packed_ref, remove_packed_ref_under_lock, Ref,
};

use super::apply::{
    apply_case_only_ref_rename, apply_reflog_rename, apply_update, ref_lock_names_for_updates,
    simulate_batch_apply, symref_deref_delete_changed_targets, RefBatchState,
};
use super::error::RefStoreError;
use super::routing::{resolve_common_dir, route_ref_storage_with_namespace, RefStorageRoute};
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PackedRefsStamp {
    len: u64,
    modified: Option<std::time::SystemTime>,
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

#[derive(Debug)]
struct PreparedFilesBatch {
    updates: Vec<RefUpdate>,
    case_only_renames: Vec<super::RefCaseOnlyRename>,
    ref_locks: HashMap<String, PathBuf>,
    packed_lock: Option<PathBuf>,
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
            },
            Err(_) => PackedRefsStamp {
                len: 0,
                modified: None,
            },
        }
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
                collect_loose_raw_refs(&dir, &format!("{storage}/"), &mut storage_map)?;
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

    fn load_raw_map_prefix(&self, logical_prefix: &str) -> Result<BTreeMap<String, RawRef>> {
        let logical_prefix = logical_prefix.trim_end_matches('/');
        let mut map: BTreeMap<String, RawRef> = BTreeMap::new();
        if logical_prefix.is_empty() {
            self.ensure_full_packed_map()?;
            let cache = self.packed_cache.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(packed) = cache.map.as_ref() {
                for (storage, oid) in packed.iter() {
                    if !self.packed_storage_visible(storage) {
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
                for (storage, oid) in read_packed_refs_map_for_prefix(base, &packed_prefix)? {
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
                    collect_loose_raw_refs(&root, storage_prefix, &mut storage_map)?;
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
                    collect_loose_raw_refs(&walk_root, &walk_prefix, &mut storage_map)?;
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
        route_ref_storage_with_namespace(&self.git_dir, refname, self.namespace_prefix.as_deref())
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

    fn acquire_ref_lock(&self, lock: &Path, refname: &str) -> StoreResult<()> {
        use crate::lockfile::{LockError, LockFile, LockWait};
        let wait = LockWait::files_ref_lock_for_git_dir(&self.common_dir);
        LockFile::acquire(lock, wait)
            .map(|guard| guard.detach())
            .map_err(|err| match err {
                LockError::Held { .. } => RefStoreError::LockHeld {
                    name: refname.to_owned(),
                },
                LockError::Io { source, .. } => RefStoreError::Corrupt(source.to_string()),
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

    fn write_loose_value(path: &Path, lock: &Path, value: &RawRef) -> Result<()> {
        remove_empty_ref_directory(path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let content = match value {
            RawRef::Direct(oid) => format!("{oid}\n"),
            RawRef::Symbolic(target) => format!("ref: {target}\n"),
        };
        fs::write(lock, content)?;
        fs::rename(lock, path)?;
        Ok(())
    }

    fn persist_case_only_loose_ref(
        &self,
        rename: &super::RefCaseOnlyRename,
        after: &RefBatchState,
        packed_lock_held: bool,
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
        self.acquire_ref_lock(&from_lock, &rename.from)
            .map_err(Error::from)?;
        let route = self.route(&rename.from);
        if !from_path.is_file() {
            if packed_lock_held {
                remove_packed_ref_under_lock(&route.storage_dir, &route.storage_name)?;
            } else {
                remove_packed_ref(&route.storage_dir, &route.storage_name)?;
            }
            FilesRefStore::write_loose_value(&from_path, &from_lock, value)?;
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

    fn delete_loose_ref(&self, refname: &str, lock: &Path, packed_lock_held: bool) -> Result<()> {
        let route = self.route(refname);
        let path = route.storage_dir.join(&route.storage_name);
        if packed_lock_held {
            remove_packed_ref_under_lock(&route.storage_dir, &route.storage_name)?;
        } else {
            remove_packed_ref(&route.storage_dir, &route.storage_name)?;
        }
        remove_empty_ref_directory(&path);
        let _ = fs::remove_file(&path);
        // Drop the lock before pruning parents: a leftover lock in the ref directory
        // would make `dir_tree_has_files` treat the parent as non-empty (issue #1317).
        let _ = fs::remove_file(lock);
        prune_empty_loose_ref_parents(&route.storage_dir, &path);
        Ok(())
    }

    fn persist_reflog(&self, name: &str, entries: &[ReflogEntry]) -> Result<()> {
        let path = self.reflog_path(name);
        if entries.is_empty() {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&path, "")?;
            return Ok(());
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
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
        fs::write(&path, body)?;
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
        let actual = if batch_differs {
            state_before.refs.get(&update.name).cloned()
        } else {
            self.read_raw(&update.name)
                .map_err(|e| RefStoreError::Corrupt(e.to_string()))?
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

    fn persist_one_ref_change(
        &self,
        name: &str,
        after: &RefBatchState,
        lock: &Path,
        packed_lock_held: bool,
    ) -> Result<()> {
        let (_, path, _) = self.ref_lock_paths(name);
        match after.refs.get(name) {
            None => self.delete_loose_ref(name, lock, packed_lock_held),
            Some(value) => FilesRefStore::write_loose_value(&path, lock, value),
        }
    }

    fn persist_one_ref_name(
        &self,
        name: &str,
        state: &RefBatchState,
        packed_lock_held: bool,
        persisted_refs: &mut HashSet<String>,
    ) -> Result<()> {
        let (_, _, lock_path) = self.ref_lock_paths(name);
        if let Some(parent) = lock_path.parent() {
            fs::create_dir_all(parent).map_err(Error::Io)?;
        }
        self.acquire_ref_lock(&lock_path, name)
            .map_err(Error::from)?;
        let write_result = self.persist_one_ref_change(name, state, &lock_path, packed_lock_held);
        let _ = fs::remove_file(&lock_path);
        write_result?;
        persisted_refs.insert(name.to_owned());
        Ok(())
    }

    /// Lock every ref `update` writes (and `update.name` when its expectation must be
    /// verified), recheck the expectation against disk under those locks, then write the refs.
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
        next: &RefBatchState,
        packed_lock_held: bool,
        persisted_refs: &mut HashSet<String>,
        writes_ref: bool,
        carried: &mut Option<(String, PathBuf)>,
    ) -> Result<()> {
        let mut targets: Vec<String> = running
            .refs
            .keys()
            .chain(next.refs.keys())
            .filter(|name| *name != &update.name && running.refs.get(*name) != next.refs.get(*name))
            .cloned()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        for target in symref_deref_delete_changed_targets(running, next, update) {
            if !persisted_refs.contains(&target) && !targets.contains(&target) {
                targets.push(target);
            }
        }
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
                Some(parent) => fs::create_dir_all(parent).map_err(Error::Io),
                None => Ok(()),
            }
            .and_then(|()| self.acquire_ref_lock(&lock_path, name).map_err(Error::from));
            if let Err(err) = acquired {
                release(&held);
                return Err(err);
            }
            held.insert(name.clone(), lock_path);
        }

        if let Err(err) = self.recheck_update_at_commit(update, batch_extras, before, running) {
            release(&held);
            return Err(err.into());
        }
        #[cfg(test)]
        tests::run_after_recheck_hook();

        for name in &targets {
            let Some(lock_path) = held.remove(name) else {
                continue;
            };
            if let Err(err) = self.persist_one_ref_change(name, next, &lock_path, packed_lock_held)
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
        before: &RefBatchState,
        after: &RefBatchState,
        packed_lock_held: bool,
        updates: &[RefUpdate],
        case_only_renames: &[super::RefCaseOnlyRename],
        carried: &mut Option<(String, PathBuf)>,
    ) -> Result<()> {
        let batch_extras: BTreeSet<String> = updates.iter().map(|u| u.name.clone()).collect();

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

        let mut persisted_refs = HashSet::new();
        for rename in case_only_renames {
            self.persist_case_only_loose_ref(rename, after, packed_lock_held)?;
            persisted_refs.insert(rename.from.clone());
            persisted_refs.insert(rename.to.clone());
        }
        let mut running = before.clone();
        for (idx, update) in updates.iter().enumerate() {
            if update.flags.log_only {
                apply_update(&mut running, update).map_err(Error::from)?;
                continue;
            }
            let mut next = running.clone();
            apply_update(&mut next, update).map_err(Error::from)?;

            let name = &update.name;
            let b = running.refs.get(name);
            let a = next.refs.get(name);
            let materialize_loose = update.new_value.is_some()
                && a.is_some()
                && b == a
                && !self
                    .loose_ref_file_exists(name)
                    .map_err(|e| RefStoreError::Corrupt(e.to_string()))?;
            let writes_ref = b != a || materialize_loose;

            self.persist_update_locked(
                update,
                &batch_extras,
                before,
                &running,
                &next,
                packed_lock_held,
                &mut persisted_refs,
                writes_ref,
                carried,
            )?;

            running = next;
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
            self.persist_one_ref_name(name, after, packed_lock_held, &mut persisted_refs)?;
        }
        self.invalidate_packed_cache_after_write();
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
        let prefix = prefix.trim_end_matches('/');
        let map = self.load_raw_map_prefix(prefix)?;
        for (name, value) in map.range(prefix.to_string()..) {
            if !name.starts_with(prefix) {
                break;
            }
            let peeled = match &value {
                RawRef::Direct(_) => None,
                RawRef::Symbolic(target) => self.resolve(target).ok(),
            };
            let entry = RefEntry {
                name: name.clone(),
                value: value.clone(),
                peeled,
            };
            if f(&entry).is_break() {
                break;
            }
        }
        Ok(())
    }

    fn prepare(
        &self,
        txn: RefTransaction,
    ) -> StoreResult<Box<dyn PreparedRefTransaction + Send + '_>> {
        RefTransaction::validate_no_duplicates(txn.updates())?;
        let (updates, reflog_renames, case_only_renames) = txn.into_parts();

        for update in &updates {
            validate_storable_refname(&update.name)?;
        }
        for rename in &reflog_renames {
            validate_storable_refname(&rename.from)?;
            validate_storable_refname(&rename.to)?;
        }
        for rename in &case_only_renames {
            validate_storable_refname(&rename.from)?;
            validate_storable_refname(&rename.to)?;
        }

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

        let mut ref_locks: HashMap<String, PathBuf> = HashMap::new();
        for name in &lock_names {
            let (_, _, lock) = self.ref_lock_paths(name);
            if let Some(parent) = lock.parent() {
                fs::create_dir_all(parent).map_err(|e| RefStoreError::Corrupt(e.to_string()))?;
            }
            if let Err(err) = self.acquire_ref_lock(&lock, name) {
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

        let before_commit = preliminary.snapshot();
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
            if let Err(err) = self.acquire_ref_lock(&lock, name) {
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
            use crate::lockfile::{LockError, LockFile, LockWait};
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
            fs::create_dir_all(parent)?;
        }
        fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(Error::Io)?;
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
        let Some(batch) = guard.take() else {
            return Ok(());
        };

        let packed_lock_held = batch.packed_lock.is_some();

        // The first update is written before any other ref, so it keeps the lock it took in
        // `prepare` instead of releasing and re-creating it. The rest are released here and
        // re-taken one update at a time, so other writers can touch later refs mid-batch.
        let mut ref_locks = batch.ref_locks;
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
        let persisted = self.store.persist_state_diff(
            &batch.before_commit,
            &batch.after_commit,
            packed_lock_held,
            &batch.updates,
            &batch.case_only_renames,
            &mut carried,
        );
        if let Some((_, lock)) = carried {
            let _ = fs::remove_file(lock);
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

fn collect_loose_raw_refs(
    dir: &Path,
    prefix: &str,
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
        let path = entry.path();
        let meta = fs::metadata(&path).map_err(Error::Io)?;
        if meta.is_dir() {
            collect_loose_raw_refs(&path, &format!("{refname}/"), out)?;
        } else if meta.is_file() {
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
}
