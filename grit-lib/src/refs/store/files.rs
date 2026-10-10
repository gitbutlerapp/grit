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
    lock_path_for_ref, prune_empty_loose_ref_parents, read_packed_refs_map, read_ref_file,
    remove_empty_ref_directory, remove_packed_ref, remove_packed_ref_under_lock, LogRefsConfig,
    Ref,
};

use super::apply::{apply_update, ref_lock_names_for_updates, simulate_batch_apply, RefBatchState};
use super::error::RefStoreError;
use super::routing::{resolve_common_dir, route_ref_storage_with_namespace, RefStorageRoute};
use super::transaction::RefTransaction;
use super::validation::{validate_storable_refname, verify_create_conflicts};
use super::{PreparedRefTransaction, RawRef, RefEntry, RefStorageFormat, RefStore, RefUpdate};

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
    /// Reflog auto-create policy for this store.
    pub log_refs: LogRefsConfig,
}

/// Loose + packed ref storage on disk.
#[derive(Debug)]
pub struct FilesRefStore {
    git_dir: PathBuf,
    common_dir: PathBuf,
    namespace_prefix: Option<String>,
    #[allow(dead_code)]
    log_refs: LogRefsConfig,
    packed_cache: Mutex<PackedCacheEntry>,
    prepared: Mutex<Option<PreparedFilesBatch>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PackedRefsStamp {
    len: u64,
    modified: Option<std::time::SystemTime>,
}

#[derive(Debug)]
struct PackedCacheEntry {
    stamp: PackedRefsStamp,
    map: HashMap<String, ObjectId>,
}

#[derive(Debug)]
struct PreparedFilesBatch {
    #[allow(dead_code)]
    lock_name: String,
    #[allow(dead_code)]
    updates: Vec<RefUpdate>,
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
        let path = config.common_dir.join("packed-refs");
        let stamp = Self::packed_refs_stamp(&path);
        let map = read_packed_refs_map(&config.common_dir).unwrap_or_default();
        Self {
            git_dir: config.git_dir,
            common_dir: config.common_dir,
            namespace_prefix: config.namespace_prefix,
            log_refs: config.log_refs,
            packed_cache: Mutex::new(PackedCacheEntry { stamp, map }),
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
            log_refs: crate::refs::effective_log_refs_config(&git_dir),
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

    fn refresh_packed_cache_if_stale(&self) -> Result<()> {
        let path = self.common_dir.join("packed-refs");
        let stamp = Self::packed_refs_stamp(&path);
        let mut guard = self.packed_cache.lock().unwrap_or_else(|e| e.into_inner());
        if guard.stamp == stamp {
            return Ok(());
        }
        guard.map = read_packed_refs_map(&self.common_dir)?;
        guard.stamp = stamp;
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
        self.refresh_packed_cache_if_stale()?;
        let cache = self.packed_cache.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(oid) = cache.map.get(&route.storage_name) {
            return Ok(Some(RawRef::Direct(*oid)));
        }
        Ok(None)
    }

    fn load_raw_map(&self) -> Result<BTreeMap<String, RawRef>> {
        self.load_raw_map_prefix("")
    }

    fn load_raw_map_prefix(&self, logical_prefix: &str) -> Result<BTreeMap<String, RawRef>> {
        self.refresh_packed_cache_if_stale()?;
        let mut map: BTreeMap<String, RawRef> = BTreeMap::new();
        let cache = self.packed_cache.lock().unwrap_or_else(|e| e.into_inner());
        for (storage, oid) in cache.map.iter() {
            let logical = self.logical_name(storage);
            if logical_prefix.is_empty() || logical.starts_with(logical_prefix) {
                map.insert(logical, RawRef::Direct(*oid));
            }
        }
        drop(cache);

        let storage_prefix = self.namespace_prefix.as_deref().unwrap_or("refs/");
        for base in [self.common_dir.as_path(), self.git_dir.as_path()] {
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
                let refs_root = base.join("refs");
                if refs_root.is_dir() {
                    let mut storage_map = BTreeMap::new();
                    collect_loose_raw_refs(&refs_root, "refs/", &mut storage_map)?;
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

    fn load_reflog_map(&self) -> Result<BTreeMap<String, Vec<ReflogEntry>>> {
        let mut out = BTreeMap::new();
        for base in [self.common_dir.as_path(), self.git_dir.as_path()] {
            let logs = base.join("logs");
            if logs.is_dir() {
                collect_reflog_refs(&logs, "", self, &mut out)?;
            }
        }
        Ok(out)
    }

    fn load_batch_state(&self) -> Result<RefBatchState> {
        Ok(RefBatchState {
            refs: self.load_raw_map()?,
            reflogs: self.load_reflog_map()?,
        })
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
        if self.refresh_packed_cache_if_stale().is_err() {
            return false;
        }
        let cache = self.packed_cache.lock().unwrap_or_else(|e| e.into_inner());
        updates.iter().any(|u| {
            u.new_value.is_none()
                && !u.flags.log_only
                && cache.map.contains_key(&self.route(&u.name).storage_name)
        })
    }

    fn acquire_ref_lock(&self, lock: &Path, refname: &str) -> StoreResult<()> {
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(lock)
        {
            Ok(_) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Err(RefStoreError::LockHeld {
                name: refname.to_owned(),
            }),
            Err(e) => Err(RefStoreError::Corrupt(e.to_string())),
        }
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
        prune_empty_loose_ref_parents(&route.storage_dir, &path);
        let _ = fs::remove_file(lock);
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

    fn persist_state_diff(
        &self,
        before: &RefBatchState,
        after: &RefBatchState,
        held_locks: &HashMap<String, PathBuf>,
        packed_lock_held: bool,
    ) -> Result<()> {
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

        for name in before
            .refs
            .keys()
            .chain(after.refs.keys())
            .collect::<BTreeSet<_>>()
        {
            let b = before.refs.get(name);
            let a = after.refs.get(name);
            if b == a && (a.is_none() || !held_locks.contains_key(name)) {
                continue;
            }
            let lock = held_locks.get(name).ok_or_else(|| {
                Error::Message(format!(
                    "ref '{name}' changed at commit without a prepare-time lock"
                ))
            })?;
            let (_, path, _) = self.ref_lock_paths(name);
            match a {
                None => {
                    self.delete_loose_ref(name, lock, packed_lock_held)?;
                }
                Some(value) => {
                    FilesRefStore::write_loose_value(&path, lock, value)?;
                }
            }
        }
        let _ = self.refresh_packed_cache_if_stale();
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
        let updates = txn.into_updates();

        for update in &updates {
            validate_storable_refname(&update.name)?;
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

        let preliminary = self
            .load_batch_state()
            .map_err(|e| RefStoreError::Corrupt(e.to_string()))?;
        verify_create_conflicts(&preliminary.refs, &updates)?;
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

        let state = self
            .load_batch_state()
            .map_err(|e| RefStoreError::Corrupt(e.to_string()))?;
        if let Err(err) = simulate_batch_apply(&state, &updates) {
            for acquired in ref_locks.values() {
                let _ = fs::remove_file(acquired);
            }
            return Err(err);
        }

        let extras: BTreeSet<String> = updates.iter().map(|u| u.name.clone()).collect();
        let skip = HashSet::new();
        for update in &updates {
            if update.new_value.is_none() || update.flags.log_only {
                continue;
            }
            if state.refs.contains_key(&update.name) {
                continue;
            }
            if let Err(reason) = crate::refs::verify_refname_available_for_create(
                &self.git_dir,
                &update.name,
                &extras,
                &skip,
            ) {
                for acquired in ref_locks.values() {
                    let _ = fs::remove_file(acquired);
                }
                return Err(reason.into());
            }
        }

        let before_commit = state.snapshot();
        let mut after_commit = before_commit.clone();
        for update in &updates {
            apply_update(&mut after_commit, update)?;
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
            let packed_path = self.common_dir.join("packed-refs");
            let lock = lock_path_for_ref(&packed_path);
            if fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&lock)
                .is_err()
            {
                for acquired in ref_locks.values() {
                    let _ = fs::remove_file(acquired);
                }
                return Err(RefStoreError::LockHeld {
                    name: "packed-refs".to_owned(),
                });
            }
            packed_lock = Some(lock);
        }

        let lock_name = updates
            .first()
            .map(|u| u.name.clone())
            .unwrap_or_else(|| "refs".to_owned());
        *prepared_guard = Some(PreparedFilesBatch {
            lock_name,
            updates,
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
        let path = self.reflog_path(name);
        let _ = fs::remove_file(path);
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

        if let Err(err) = self.store.persist_state_diff(
            &batch.before_commit,
            &batch.after_commit,
            &batch.ref_locks,
            packed_lock_held,
        ) {
            FilesRefStore::release_locks(&batch);
            return Err(RefStoreError::Corrupt(err.to_string()));
        }
        FilesRefStore::release_locks(&batch);
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
    }
}

fn head_matches_prefix(logical_prefix: &str) -> bool {
    logical_prefix.is_empty()
        || "HEAD".starts_with(logical_prefix)
        || logical_prefix.starts_with("HEAD")
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

fn collect_reflog_refs(
    dir: &Path,
    prefix: &str,
    store: &FilesRefStore,
    out: &mut BTreeMap<String, Vec<ReflogEntry>>,
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
            collect_reflog_refs(&path, &format!("{refname}/"), store, out)?;
        } else if path.is_file() {
            let logical = store.logical_name(&refname);
            let entries = read_reflog_at_path(&path)?;
            if !entries.is_empty() {
                out.insert(logical, entries);
            }
        }
    }
    Ok(())
}

fn read_reflog_at_path(path: &Path) -> Result<Vec<ReflogEntry>> {
    let content = match fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
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
