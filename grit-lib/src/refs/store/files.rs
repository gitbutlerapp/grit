//! Filesystem ref backend (loose refs + `packed-refs`) implementing [`super::RefStore`].
//!
//! On Windows, [`FilesRefStore`] rejects ref updates whose on-disk path would use a reserved
//! device name (`CON`, `AUX`, `COM1`, …) or another component Win32 cannot represent without
//! the `\\?\` prefix, because system Git cannot read such loose refs or reflogs. Deletes of
//! refs that were already written remain allowed so grit can remove broken entries. Reftable
//! and other backends are unchanged.

use crate::git_path::CanonicalizeExt;
use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs;
use std::io;
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::error::{Error, Result};
use crate::git_dir::io_error::{
    io_with_ref_store_path, map_ref_store_path_io, GitDirPathOperation,
};
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
use super::backend::{is_per_ref_rejectable, partition_updates_allowing_failures};
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
///
/// On Windows, updates whose storage path includes a reserved device name or another
/// non-round-tripping path component are rejected with [`RefStoreError::InvalidRefName`]
/// so repositories stay readable by system Git.
#[derive(Debug)]
pub struct FilesRefStore {
    git_dir: PathBuf,
    common_dir: PathBuf,
    namespace_prefix: Option<String>,
    packed_cache: Mutex<PackedCacheEntry>,
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
    /// `packed-refs` records an initial transaction adds instead of loose files (see
    /// [`RefTransaction::initial`]); empty for every other batch.
    initial_packed_records: Vec<String>,
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
        }
    }

    /// Git directory this store reads `HEAD` and per-worktree refs from.
    pub(crate) fn git_dir(&self) -> &Path {
        &self.git_dir
    }

    /// Convenience constructor for a simple repository directory.
    pub fn from_git_dir(git_dir: impl Into<PathBuf>) -> Result<Self> {
        let git_dir: PathBuf = git_dir.into();
        let git_dir = git_dir.canonicalize_simplified().unwrap_or(git_dir);
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
            file.full_map(&path)?
        } else {
            BTreeMap::new()
        };
        guard.map = Some(map);
        guard.map_is_complete = true;
        Ok(())
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
        let packed = self.packed_refs_view()?;
        // Siblings share their ancestors; look each one up once per batch.
        let mut not_refs: HashSet<&str> = HashSet::new();
        for name in creates {
            for (slash, _) in name.match_indices('/') {
                let ancestor = &name[..slash];
                if refs.contains_key(ancestor) || not_refs.contains(ancestor) {
                    continue;
                }
                match self.existing_ref_named_in(ancestor, packed.as_ref(), None)? {
                    Some(raw) => {
                        refs.insert(ancestor.to_owned(), raw);
                    }
                    None => {
                        not_refs.insert(ancestor);
                    }
                }
            }
            for (descendant, raw) in self.descendant_refs_in(name, packed.as_ref())? {
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
    ///
    /// `held_lock` is the path of `name`'s lock file when the caller holds it: every path above
    /// it is then a directory, so ancestors are not probed for loose files there.
    fn verify_create_available(
        &self,
        name: &str,
        extras: &BTreeSet<&str>,
        held_lock: Option<&Path>,
    ) -> std::result::Result<(), RefnameUnavailable> {
        let packed = self.packed_refs_view().ok().flatten();
        self.verify_create_available_in(
            name,
            extras,
            held_lock,
            packed.as_ref(),
            &mut HashMap::new(),
        )
    }

    /// [`Self::verify_create_available`] against one view of `packed-refs` (`packed`), with
    /// `ancestors` remembering which ancestor names are refs across the creates of a batch.
    fn verify_create_available_in(
        &self,
        name: &str,
        extras: &BTreeSet<&str>,
        held_lock: Option<&Path>,
        packed: Option<&crate::refs::PackedRefsFileView>,
        ancestors: &mut HashMap<String, bool>,
    ) -> std::result::Result<(), RefnameUnavailable> {
        let route = self.route(name);
        for base in self.store_bases() {
            let ref_path = base.join(&route.storage_name);
            if ref_path.is_dir() && crate::refs::dir_tree_has_files(&ref_path) {
                let blocking = first_loose_ref_file_under(&ref_path)
                    .map(|p| loose_storage_path_to_refname(base, &p))
                    .unwrap_or_else(|| format!("{name}/…"));
                return Err(RefnameUnavailable::DescendantExists {
                    blocking,
                    new_ref: name.to_owned(),
                });
            }
        }
        let mut blocking = BTreeSet::new();
        for (slash, _) in name.match_indices('/') {
            let ancestor = &name[..slash];
            let is_ref = match ancestors.get(ancestor) {
                Some(&is_ref) => is_ref,
                None => {
                    let is_ref = matches!(
                        self.existing_ref_named_in(ancestor, packed, held_lock),
                        Ok(Some(_))
                    );
                    ancestors.insert(ancestor.to_owned(), is_ref);
                    is_ref
                }
            };
            if is_ref {
                blocking.insert(ancestor.to_owned());
            }
        }
        let storage = self.route(name).storage_name;
        let mut storage_names = BTreeSet::new();
        for base in self.store_bases() {
            collect_loose_file_names(&base.join(&storage), &storage, &mut storage_names);
        }
        if let Some(packed) = packed.filter(|_| storage.contains('/')) {
            let path = super::paths::packed_refs_path(&self.common_dir);
            if let Ok(Some((descendant, _))) = packed.first_descendant(&path, &storage) {
                storage_names.insert(descendant);
            }
        }
        blocking.extend(storage_names.iter().map(|s| self.logical_name(s)));
        verify_name_available_for_create(name, &blocking, extras)
    }

    /// Directories holding loose refs and logs: the common dir, then the per-worktree
    /// git dir when it differs (a linked worktree). The main worktree yields one entry,
    /// so its loose refs are not walked twice.
    fn store_bases(&self) -> impl Iterator<Item = &Path> {
        let common = self.common_dir.as_path();
        let git = self.git_dir.as_path();
        std::iter::once(common).chain((git != common).then_some(git))
    }

    /// The value of `name` as a full listing would report it: a parseable loose file in either
    /// store directory, else a visible packed record. Directories and unparseable files are
    /// not refs.
    ///
    /// `packed` is one view of `packed-refs` shared by several lookups, so the file is
    /// revalidated once. Loose paths above `held_lock`, a lock file the caller holds, are
    /// directories and are not probed.
    fn existing_ref_named_in(
        &self,
        name: &str,
        packed: Option<&crate::refs::PackedRefsFileView>,
        held_lock: Option<&Path>,
    ) -> Result<Option<RawRef>> {
        let storage = self.route(name).storage_name;
        for base in self.store_bases() {
            let path = base.join(&storage);
            if held_lock.is_some_and(|lock| lock.starts_with(&path)) || !path.is_file() {
                continue;
            }
            if let Ok(r) = read_ref_file(&path) {
                return Ok(Some(ref_to_raw(r)));
            }
        }
        let Some(packed) = packed.filter(|_| self.packed_storage_visible(&storage)) else {
            return Ok(None);
        };
        let path = super::paths::packed_refs_path(&self.common_dir);
        Ok(packed.lookup_oid(&path, &storage)?.map(RawRef::Direct))
    }

    /// Loose refs under `<name>/` in either store directory, plus the first one in `packed`.
    fn descendant_refs_in(
        &self,
        name: &str,
        packed: Option<&crate::refs::PackedRefsFileView>,
    ) -> Result<BTreeMap<String, RawRef>> {
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
        if let Some(packed) = packed.filter(|_| storage.contains('/')) {
            let path = super::paths::packed_refs_path(&self.common_dir);
            if let Some((descendant, oid)) = packed.first_descendant(&path, &storage)? {
                if self.packed_storage_visible(&descendant) {
                    storage_map.entry(descendant).or_insert(RawRef::Direct(oid));
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

    /// Make room for a new reflog of `name`: an empty directory tree at its path is removed,
    /// and one that still holds logs (left behind for `<name>/…` refs) blocks the write.
    ///
    /// # Errors
    ///
    /// [`RefStoreError::NameUnavailable`] naming the first log under the directory, or
    /// [`RefStoreError::Corrupt`] when the directory cannot be listed.
    fn ensure_reflog_path_writable(&self, name: &str) -> StoreResult<()> {
        let path = self.reflog_path(name);
        let is_dir = |path: &Path| fs::symlink_metadata(path).is_ok_and(|m| m.is_dir());
        if !is_dir(&path) {
            return Ok(());
        }
        crate::refs::remove_empty_ref_directory(&path);
        if !is_dir(&path) {
            return Ok(());
        }
        let storage = self.route(name).storage_name;
        let mut logs = BTreeSet::new();
        collect_loose_file_names(&path, &storage, &mut logs);
        let Some(log) = logs.first() else {
            return Err(RefStoreError::Corrupt(format!(
                "cannot write reflog at '{}': unreadable directory in the way",
                path.display()
            )));
        };
        Err(RefnameUnavailable::DescendantExists {
            blocking: self.logical_name(log),
            new_ref: name.to_owned(),
        }
        .into())
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
            for (i, base) in self.store_bases().enumerate() {
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

        for base in self.store_bases() {
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

    /// Reject updates that would write a loose ref or reflog path Win32 cannot expose to
    /// system Git (reserved device names and similar). Deletes are left alone.
    #[cfg(windows)]
    fn reject_win32_unstorable_ref_paths(&self, txn: &RefTransaction) -> StoreResult<()> {
        use crate::git_path::win32_storage_path_components_round_trip;

        let storage_ok =
            |name: &str| win32_storage_path_components_round_trip(&self.route(name).storage_name);
        for update in txn.updates() {
            if update.new_value.is_none() && !update.flags.log_only {
                continue;
            }
            if !storage_ok(&update.name) {
                return Err(RefStoreError::InvalidRefName {
                    name: update.name.clone(),
                });
            }
        }
        for rename in txn.reflog_renames() {
            if !storage_ok(&rename.to) {
                return Err(RefStoreError::InvalidRefName {
                    name: rename.to.clone(),
                });
            }
        }
        for rename in txn.case_only_renames() {
            if !storage_ok(&rename.to) {
                return Err(RefStoreError::InvalidRefName {
                    name: rename.to.clone(),
                });
            }
        }
        Ok(())
    }

    fn logical_name(&self, storage: &str) -> String {
        crate::ref_namespace::logical_ref_name_with_prefix(
            self.namespace_prefix.as_deref(),
            storage,
        )
    }

    /// Path of the loose reflog file for the logical ref name `logical`.
    pub(crate) fn reflog_path(&self, logical: &str) -> PathBuf {
        let route = self.route(logical);
        route.storage_dir.join("logs").join(&route.storage_name)
    }

    /// Every parseable entry of `name`'s reflog, oldest first, without the per-entry clone
    /// [`RefStore::for_each_reflog_entry`] needs; empty when the log is missing.
    pub(crate) fn read_reflog_entries(&self, name: &str) -> Result<Vec<ReflogEntry>> {
        read_reflog_at_path(&self.reflog_path(name))
    }

    fn ref_lock_paths(&self, refname: &str) -> (RefStorageRoute, PathBuf, PathBuf) {
        let route = self.route(refname);
        let path = route.storage_dir.join(&route.storage_name);
        let lock = lock_path_for_ref(&path);
        (route, path, lock)
    }

    /// The `packed-refs` records (ref line plus `^` peel line for annotated tags) that store
    /// an initial transaction's updates, or `None` when the batch must commit as usual: an
    /// update is not a plain direct create, stores outside the common directory's
    /// `packed-refs`, or names a ref that exists already.
    ///
    /// # Errors
    ///
    /// [`RefStoreError::NameUnavailable`] when the batch conflicts with itself or with the
    /// stored refs (as the regular path would report), and listing failures.
    fn initial_packed_records(&self, txn: &RefTransaction) -> StoreResult<Option<Vec<String>>> {
        let updates = txn.updates();
        if updates.is_empty()
            || !txn.reflog_renames().is_empty()
            || !txn.case_only_renames().is_empty()
        {
            return Ok(None);
        }
        let mut creates = Vec::with_capacity(updates.len());
        for update in updates {
            let Some(RawRef::Direct(oid)) = update.new_value else {
                return Ok(None);
            };
            if update.flags.log_only
                || update.reflog.is_some()
                || !matches!(update.expected, Expected::Any | Expected::Missing)
            {
                return Ok(None);
            }
            let route = self.route(&update.name);
            if route.storage_dir != self.common_dir || route.storage_name != update.name {
                return Ok(None);
            }
            creates.push((update.name.as_str(), oid));
        }
        let existing = self
            .load_raw_map_prefix("", None)
            .map_err(|e| RefStoreError::Corrupt(e.to_string()))?;
        if updates.iter().any(|u| existing.contains_key(&u.name)) {
            return Ok(None);
        }
        verify_create_conflicts(&existing, updates)?;

        let odb = crate::odb::Odb::new(&self.common_dir.join("objects"))
            .with_config_git_dir(self.git_dir.clone());
        let mut peels: HashMap<ObjectId, Option<ObjectId>> = HashMap::new();
        let records = odb.with_pack_read_context(|| {
            creates
                .into_iter()
                .map(|(name, oid)| {
                    let peel = *peels
                        .entry(oid)
                        .or_insert_with(|| crate::refs::packed_ref_peel(&odb, oid));
                    match peel {
                        Some(peel) => format!("{oid} {name}\n^{peel}\n"),
                        None => format!("{oid} {name}\n"),
                    }
                })
                .collect()
        });
        Ok(Some(records))
    }

    /// Take `packed-refs.lock` for a batch, waiting as `core.packedRefsTimeout` allows.
    fn acquire_packed_lock(&self) -> StoreResult<PathBuf> {
        use crate::lockfile::{LockError, LockFile};
        let packed_path = super::paths::packed_refs_path(&self.common_dir);
        let lock = lock_path_for_ref(&packed_path);
        let wait = LockWait::packed_refs_for_git_dir(&self.common_dir);
        match LockFile::acquire(&lock, wait).map(|guard| guard.detach()) {
            Ok(()) => Ok(lock),
            Err(LockError::Held { .. }) => Err(RefStoreError::LockHeld {
                name: super::paths::PACKED_REFS.to_owned(),
            }),
            Err(LockError::Io { source, .. }) => Err(RefStoreError::Corrupt(source.to_string())),
        }
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

    /// Take the locks of `names` that `prepare` holds, recording them in `ref_locks`: the
    /// `carried` ref (the first update's, which commit keeps writing under), and every other
    /// ref whose lock path is not plainly absent right now, so a lock held elsewhere is waited
    /// for, and a path that cannot hold a lock is reported, before anything is written.
    ///
    /// Commit re-takes the other locks one update at a time, so creating and removing each
    /// uncontended lock here would only add a file create and unlink per ref.
    ///
    /// # Errors
    ///
    /// The failure of [`Self::lock_ref`]; every lock in `ref_locks` is released first.
    fn take_prepare_locks<'a>(
        &self,
        names: impl IntoIterator<Item = &'a String>,
        carried: Option<&str>,
        wait: LockWait,
        ref_locks: &mut HashMap<String, PathBuf>,
    ) -> StoreResult<()> {
        for name in names {
            if ref_locks.contains_key(name) {
                continue;
            }
            let (_, _, lock) = self.ref_lock_paths(name);
            // Only a lock path that is really absent can wait for commit. Anything else (a
            // file where a parent directory belongs, a permission error) must fail here,
            // before the batch writes any ref.
            let absent = matches!(
                fs::symlink_metadata(&lock),
                Err(err) if err.kind() == io::ErrorKind::NotFound
            );
            if carried != Some(name.as_str()) && absent {
                continue;
            }
            if let Err(err) = self.lock_ref(name, &lock, wait) {
                for acquired in ref_locks.values() {
                    let _ = fs::remove_file(acquired);
                }
                return Err(Self::ref_store_error_from_lib(err));
            }
            ref_locks.insert(name.clone(), lock);
        }
        Ok(())
    }

    /// Take `refname`'s lock file at `lock_path`, creating its directory when needed.
    ///
    /// # Errors
    ///
    /// A directory that cannot be created (a ref file in the way) is reported as a
    /// [`GitDirPathOperation::CreateDir`] failure; otherwise as [`Self::acquire_ref_lock_mapped`].
    fn lock_ref(&self, refname: &str, lock_path: &Path, wait: LockWait) -> Result<()> {
        if let Some(reason) = self.name_unavailable_if_ancestor_is_loose_file(refname) {
            return Err(Error::RefStore(RefStoreError::NameUnavailable { reason }));
        }
        let Err(first_err) = self.acquire_ref_lock_mapped(lock_path, refname, wait) else {
            return Ok(());
        };
        if matches!(first_err, Error::RefStore(RefStoreError::LockHeld { .. })) {
            return Err(first_err);
        }
        if !Self::lock_acquire_suggests_missing_parent(&first_err) {
            return self.map_lock_io_to_name_unavailable(refname, lock_path, first_err);
        }
        if let Some(parent) = lock_path.parent() {
            if parent.is_file() {
                return Err(Error::RefStore(RefStoreError::NameUnavailable {
                    reason: RefnameUnavailable::AncestorExists {
                        blocking: loose_storage_path_to_refname(&self.common_dir, parent),
                        new_ref: refname.to_owned(),
                    },
                }));
            }
            if let Err(source) = fs::create_dir_all(parent) {
                if let Some(reason) = self.name_unavailable_if_ancestor_is_loose_file(refname) {
                    return Err(Error::RefStore(RefStoreError::NameUnavailable { reason }));
                }
                self.io_ref_store(parent, GitDirPathOperation::CreateDir, Err(source))?;
            }
            if parent.is_file() {
                return Err(Error::RefStore(RefStoreError::NameUnavailable {
                    reason: RefnameUnavailable::AncestorExists {
                        blocking: loose_storage_path_to_refname(&self.common_dir, parent),
                        new_ref: refname.to_owned(),
                    },
                }));
            }
        }
        if let Some(reason) = self.name_unavailable_if_ancestor_is_loose_file(refname) {
            return Err(Error::RefStore(RefStoreError::NameUnavailable { reason }));
        }
        match self.acquire_ref_lock_mapped(lock_path, refname, wait) {
            Ok(()) => Ok(()),
            Err(err) => self.map_lock_io_to_name_unavailable(refname, lock_path, err),
        }
    }

    fn map_lock_io_to_name_unavailable(
        &self,
        refname: &str,
        lock_path: &Path,
        err: Error,
    ) -> Result<()> {
        let (_, ref_path, _) = self.ref_lock_paths(refname);
        if let Some(reason) = name_unavailable_if_lock_path_io_blocked(
            &self.common_dir,
            refname,
            &ref_path,
            lock_path,
            &err,
        ) {
            Err(Error::RefStore(RefStoreError::NameUnavailable { reason }))
        } else {
            Err(err)
        }
    }

    /// Whether a failed lock acquisition likely means the lock path's parent directory is missing.
    fn lock_acquire_suggests_missing_parent(err: &Error) -> bool {
        match err {
            Error::GitDirPathIo { source, .. } => source.kind() == io::ErrorKind::NotFound,
            Error::Io(source) => source.kind() == io::ErrorKind::NotFound,
            _ => false,
        }
    }

    /// Whether a strict ancestor of `refname` is already stored as a loose ref file (blocks
    /// creating a descendant under it).
    fn name_unavailable_if_ancestor_is_loose_file(
        &self,
        refname: &str,
    ) -> Option<RefnameUnavailable> {
        for (slash, _) in refname.match_indices('/').rev() {
            let ancestor = &refname[..slash];
            if ancestor == "refs" {
                continue;
            }
            if self.loose_ref_file_exists(ancestor).ok()? {
                return Some(RefnameUnavailable::AncestorExists {
                    blocking: ancestor.to_owned(),
                    new_ref: refname.to_owned(),
                });
            }
        }
        None
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
                LockError::Io { source, path } => map_ref_store_path_io(
                    &self.common_dir,
                    &self.git_dir,
                    path.as_path(),
                    GitDirPathOperation::UpdateRef,
                    source,
                ),
            })
    }

    fn ref_store_error_from_lib(err: Error) -> RefStoreError {
        match err {
            Error::GitDirPathIo {
                path,
                operation,
                source,
            } => RefStoreError::git_dir_path_io(path, operation, source),
            Error::RefStore(store_err) => store_err,
            other => RefStoreError::Corrupt(other.to_string()),
        }
    }

    fn io_ref_store<T>(
        &self,
        path: &Path,
        operation: GitDirPathOperation,
        result: io::Result<T>,
    ) -> Result<T> {
        io_with_ref_store_path(&self.common_dir, &self.git_dir, path, operation, result)
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
        common_dir: &Path,
        checkout_git_dir: &Path,
        refname: &str,
        path: &Path,
        lock: &Path,
        value: &RawRef,
    ) -> Result<()> {
        if let Some(parent) = lock.parent() {
            if parent.is_file() {
                return Err(Error::RefStore(RefStoreError::NameUnavailable {
                    reason: RefnameUnavailable::AncestorExists {
                        blocking: loose_storage_path_to_refname(common_dir, parent),
                        new_ref: refname.to_owned(),
                    },
                }));
            }
        }
        if path.is_dir() && crate::refs::dir_tree_has_files(path) {
            return Err(Error::RefStore(RefStoreError::NameUnavailable {
                reason: name_unavailable_after_loose_rename_conflict(refname, path, common_dir),
            }));
        }
        let content = match value {
            RawRef::Direct(oid) => format!("{oid}\n"),
            RawRef::Symbolic(target) => format!("ref: {target}\n"),
        };
        match io_with_ref_store_path(
            common_dir,
            checkout_git_dir,
            lock,
            GitDirPathOperation::UpdateRef,
            fs::write(lock, content),
        ) {
            Ok(()) => {}
            Err(err) => {
                if let Some(reason) =
                    name_unavailable_if_lock_path_io_blocked(common_dir, refname, path, lock, &err)
                {
                    let _ = fs::remove_file(lock);
                    return Err(Error::RefStore(RefStoreError::NameUnavailable { reason }));
                }
                return Err(err);
            }
        }
        // An empty directory tree left by deleted nested refs may occupy the ref's path.
        let renamed = fs::rename(lock, path).or_else(|err| {
            if !path.is_dir() {
                return Err(err);
            }
            remove_empty_ref_directory(path);
            fs::rename(lock, path)
        });
        match renamed {
            Ok(()) => Ok(()),
            Err(err)
                if matches!(
                    err.kind(),
                    io::ErrorKind::IsADirectory | io::ErrorKind::NotADirectory
                ) =>
            {
                let _ = fs::remove_file(lock);
                Err(Error::RefStore(RefStoreError::NameUnavailable {
                    reason: name_unavailable_after_loose_rename_conflict(refname, path, common_dir),
                }))
            }
            Err(err) => io_with_ref_store_path(
                common_dir,
                checkout_git_dir,
                path,
                GitDirPathOperation::UpdateRef,
                Err(err),
            ),
        }
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
        self.acquire_ref_lock_mapped(&from_lock, &rename.from, ctx.ref_lock_wait)?;
        if !from_path.is_file() {
            self.remove_packed_record(&rename.from, ctx)?;
            FilesRefStore::write_loose_value(
                &self.common_dir,
                &self.git_dir,
                &rename.from,
                &from_path,
                &from_lock,
                value,
            )?;
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

    /// Replace a reflog file atomically (temp file in the same directory, then rename).
    fn persist_reflog_atomic(&self, name: &str, entries: &[ReflogEntry]) -> Result<()> {
        let path = self.reflog_path(name);
        if let Some(parent) = path.parent() {
            self.io_ref_store(
                parent,
                GitDirPathOperation::CreateDir,
                fs::create_dir_all(parent),
            )?;
        }
        let body: String = if entries.is_empty() {
            String::new()
        } else {
            entries.iter().map(reflog_line).collect()
        };
        let tmp = path.with_extension("new");
        let mut created_tmp = false;
        let write_result = (|| -> Result<()> {
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&tmp)
                .map_err(Error::Io)?;
            created_tmp = true;
            use std::io::Write as _;
            file.write_all(body.as_bytes()).map_err(Error::Io)?;
            drop(file);
            self.io_ref_store(
                &path,
                GitDirPathOperation::AppendReflog,
                fs::rename(&tmp, &path),
            )?;
            created_tmp = false;
            Ok(())
        })();
        if write_result.is_err() && created_tmp {
            let _ = fs::remove_file(&tmp);
        }
        write_result
    }

    /// Add `entries` to the end of `name`'s reflog, leaving the lines it has as they are
    /// (lines this store can't parse, and messages that are not UTF-8, included), as Git
    /// appends to a log. A log whose last line has no line ending gets one first.
    ///
    /// # Returns
    ///
    /// `false` when the log is gone, so nothing was written.
    fn append_reflog_entries(&self, name: &str, entries: &[ReflogEntry]) -> Result<bool> {
        use std::io::{Read, Seek, SeekFrom, Write};

        let path = self.reflog_path(name);
        let mut file = match fs::OpenOptions::new().read(true).append(true).open(&path) {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(e) => {
                return self.io_ref_store(&path, GitDirPathOperation::AppendReflog, Err(e));
            }
        };
        let mut body = String::new();
        if file.metadata()?.len() > 0 {
            let mut last = [0u8; 1];
            file.seek(SeekFrom::End(-1))?;
            file.read_exact(&mut last)?;
            if last[0] != b'\n' {
                body.push('\n');
            }
        }
        body.extend(entries.iter().map(reflog_line));
        self.io_ref_store(
            &path,
            GitDirPathOperation::AppendReflog,
            file.write_all(body.as_bytes()),
        )?;
        Ok(true)
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

    /// Verify `update` against disk under its locks: its expectation, and for a create that
    /// the name is still free. `held_lock` is `update.name`'s lock file when it is held.
    fn recheck_update_at_commit(
        &self,
        update: &RefUpdate,
        batch_extras: &BTreeSet<&str>,
        batch_start: &RefBatchState,
        state_before: &RefBatchState,
        held_lock: Option<&Path>,
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
            self.verify_create_available(&update.name, batch_extras, held_lock)?;
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
        let (_, path, _) = self.ref_lock_paths(name);
        match value {
            None => self.delete_loose_ref(name, lock, ctx),
            Some(value) => FilesRefStore::write_loose_value(
                &self.common_dir,
                &self.git_dir,
                name,
                &path,
                lock,
                value,
            ),
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
        self.lock_ref(name, &lock_path, ctx.ref_lock_wait)?;
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
        batch_extras: &BTreeSet<&str>,
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
            if let Err(err) = self.lock_ref(name, &lock_path, ctx.ref_lock_wait) {
                release(&held);
                return Err(err);
            }
            held.insert(name.clone(), lock_path);
        }

        let held_lock = held.get(&update.name).map(PathBuf::as_path);
        if let Err(err) =
            self.recheck_update_at_commit(update, batch_extras, before, running, held_lock, ctx)
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
        let batch_extras: BTreeSet<&str> = updates.iter().map(|u| u.name.as_str()).collect();
        if updates.iter().any(|u| {
            u.new_value.is_some() && !u.flags.log_only && !before.refs.contains_key(&u.name)
        }) {
            self.forget_packed_cache();
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

        let names: BTreeSet<&String> = before.reflogs.keys().chain(after.reflogs.keys()).collect();
        for name in names {
            let (old, new) = (before.reflogs.get(name), after.reflogs.get(name));
            if old == new {
                continue;
            }
            let under_carried = carried
                .as_ref()
                .is_some_and(|(carried_name, _)| carried_name == name);
            let mut extra_lock = None;
            if !under_carried {
                let (_, _, lock_path) = self.ref_lock_paths(name);
                self.lock_ref(name, &lock_path, ctx.ref_lock_wait)?;
                extra_lock = Some(lock_path);
            }
            let write_result = (|| -> Result<()> {
                if let (Some(old), Some(new)) = (old, new) {
                    if new.starts_with(old)
                        && self.append_reflog_entries(name, &new[old.len()..])?
                    {
                        return Ok(());
                    }
                }
                self.persist_reflog_atomic(name, new.map(Vec::as_slice).unwrap_or_default())
            })();
            if let Some(lock_path) = extra_lock {
                let _ = fs::remove_file(&lock_path);
            }
            write_result?;
        }

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
        batch_extras: &BTreeSet<&str>,
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

        if txn.is_initial() {
            if let Some(initial_packed_records) = self.initial_packed_records(&txn)? {
                let packed_lock = self.acquire_packed_lock()?;
                return Ok(Box::new(FilesPrepared {
                    store: self,
                    committed: false,
                    batch: Some(PreparedFilesBatch {
                        updates: Vec::new(),
                        case_only_renames: Vec::new(),
                        ref_locks: HashMap::new(),
                        packed_lock: Some(packed_lock),
                        ref_lock_wait: LockWait::Immediately,
                        before_commit: RefBatchState::default(),
                        after_commit: RefBatchState::default(),
                        initial_packed_records,
                    }),
                    rejections: Vec::new(),
                }));
            }
        }

        #[cfg(windows)]
        self.reject_win32_unstorable_ref_paths(&txn)?;

        let allow_failures = txn.allows_failures();
        let (updates, reflog_renames, case_only_renames) = txn.into_parts();
        let mut rejections = Vec::new();

        let mut preliminary = self
            .load_batch_state_for_prepare(&updates, &reflog_renames, &case_only_renames)
            .map_err(|e| RefStoreError::Corrupt(e.to_string()))?;
        self.insert_create_conflict_neighbors(&mut preliminary.refs, &updates)
            .map_err(|e| RefStoreError::Corrupt(e.to_string()))?;
        let updates = if allow_failures {
            let (accepted, part) = partition_updates_allowing_failures(&preliminary, updates)?;
            rejections.extend(part);
            accepted
        } else {
            verify_create_conflicts(&preliminary.refs, &updates)?;
            updates
        };
        for rename in &reflog_renames {
            self.ensure_reflog_path_writable(&rename.to)?;
        }
        for rename in &case_only_renames {
            self.ensure_reflog_path_writable(&rename.to)?;
        }
        let lock_names = ref_lock_names_for_updates(&preliminary, &updates);
        let ref_lock_wait = LockWait::files_ref_lock_for_git_dir(&self.common_dir);

        let carried = updates
            .iter()
            .find(|u| !u.flags.log_only)
            .map(|u| u.name.as_str());
        let mut ref_locks: HashMap<String, PathBuf> = HashMap::new();
        self.take_prepare_locks(&lock_names, carried, ref_lock_wait, &mut ref_locks)?;

        if let Err(err) =
            simulate_batch_apply(&preliminary, &updates, &reflog_renames, &case_only_renames)
        {
            for acquired in ref_locks.values() {
                let _ = fs::remove_file(acquired);
            }
            return Err(err);
        }

        let extras: BTreeSet<&str> = updates.iter().map(|u| u.name.as_str()).collect();
        let packed = self.packed_refs_view().ok().flatten();
        let mut ancestors = HashMap::new();
        let mut available_updates = Vec::with_capacity(updates.len());
        for update in &updates {
            if update.new_value.is_none() || update.flags.log_only {
                available_updates.push(update.clone());
                continue;
            }
            if preliminary.refs.contains_key(&update.name) {
                available_updates.push(update.clone());
                continue;
            }
            if let Err(reason) = self.verify_create_available_in(
                &update.name,
                &extras,
                ref_locks.get(&update.name).map(PathBuf::as_path),
                packed.as_ref(),
                &mut ancestors,
            ) {
                let err: RefStoreError = reason.into();
                if allow_failures && is_per_ref_rejectable(&err) {
                    rejections.push((update.name.clone(), err));
                    continue;
                }
                for acquired in ref_locks.values() {
                    let _ = fs::remove_file(acquired);
                }
                return Err(err);
            }
            available_updates.push(update.clone());
        }
        let updates = available_updates;

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
                changed.insert(&update.name);
            }
        }
        // Commit writes reflogs before refs, so a directory where a new log goes must stop
        // the batch here, not after some of it is written.
        for name in after_commit.reflogs.keys() {
            if before_commit.reflogs.contains_key(name) {
                continue;
            }
            if let Err(err) = self.ensure_reflog_path_writable(name) {
                for acquired in ref_locks.values() {
                    let _ = fs::remove_file(acquired);
                }
                return Err(err);
            }
        }

        self.take_prepare_locks(changed, carried, ref_lock_wait, &mut ref_locks)?;

        let mut packed_lock = None;
        if self.needs_packed_lock(&updates) {
            match self.acquire_packed_lock() {
                Ok(lock) => packed_lock = Some(lock),
                Err(err) => {
                    for acquired in ref_locks.values() {
                        let _ = fs::remove_file(acquired);
                    }
                    return Err(err);
                }
            }
        }

        Ok(Box::new(FilesPrepared {
            store: self,
            committed: false,
            rejections: rejections.clone(),
            batch: Some(PreparedFilesBatch {
                updates,
                case_only_renames,
                ref_locks,
                packed_lock,
                ref_lock_wait,
                before_commit,
                after_commit,
                initial_packed_records: Vec::new(),
            }),
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
        let path = self.reflog_path(name);
        if reverse {
            // Newest-first readers usually stop early, so read the log backwards in chunks.
            let Some(mut reader) = reflog::ReverseReflogLineReader::open(&path)? else {
                return Ok(());
            };
            while let Some(line) = reader.next_nonempty_line()? {
                let Some(entry) = reflog::parse_reflog_line_for_store(&utf8_or_lossy(&line)) else {
                    continue;
                };
                if f(&entry).is_break() {
                    break;
                }
            }
            return Ok(());
        }
        for entry in &read_reflog_at_path(&path)? {
            if f(entry).is_break() {
                break;
            }
        }
        Ok(())
    }

    fn create_reflog(&self, name: &str) -> Result<()> {
        validate_storable_refname(name).map_err(|e| Error::Message(e.to_string()))?;
        let path = self.reflog_path(name);
        if let Some(parent) = path.parent() {
            self.io_ref_store(
                parent,
                GitDirPathOperation::CreateDir,
                fs::create_dir_all(parent),
            )?;
        }
        self.io_ref_store(
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
        self.rewrite_reflog(name, &mut |_| Ok(entries.clone()))
    }

    fn rewrite_reflog(
        &self,
        name: &str,
        transform: &mut dyn FnMut(Vec<ReflogEntry>) -> Result<Vec<ReflogEntry>>,
    ) -> Result<()> {
        let wait = LockWait::files_ref_lock_for_git_dir(&self.common_dir);
        let (_, _, lock_path) = self.ref_lock_paths(name);
        self.lock_ref(name, &lock_path, wait)?;
        let result = (|| -> Result<()> {
            let entries = self.read_reflog_entries(name)?;
            let rewritten = transform(entries)?;
            self.persist_reflog_atomic(name, &rewritten)
        })();
        let _ = fs::remove_file(&lock_path);
        result
    }

    fn for_each_reflog_ref(&self, f: &mut dyn FnMut(&str) -> ControlFlow<()>) -> Result<()> {
        let mut names = BTreeSet::new();
        for base in self.store_bases() {
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
    batch: Option<PreparedFilesBatch>,
    rejections: Vec<(String, RefStoreError)>,
}

impl PreparedRefTransaction for FilesPrepared<'_> {
    fn rejected_updates(&self) -> &[(String, RefStoreError)] {
        &self.rejections
    }

    fn commit(mut self: Box<Self>) -> StoreResult<()> {
        let Some(mut batch) = self.batch.take() else {
            return Ok(());
        };

        if !batch.initial_packed_records.is_empty() {
            let records: Vec<&str> = batch
                .initial_packed_records
                .iter()
                .map(String::as_str)
                .collect();
            let written = restore_packed_records_under_lock(&self.store.common_dir, &records);
            FilesRefStore::release_locks(&batch);
            self.store.invalidate_packed_cache_after_write();
            written.map_err(FilesRefStore::ref_store_error_from_lib)?;
            self.committed = true;
            return Ok(());
        }

        // The first update is written before any other ref, so it keeps the lock it took in
        // `prepare` instead of releasing and re-creating it. Any other lock `prepare` took (only
        // ones another writer held) is released here; each ref is locked again when its update
        // is written, so other writers can touch later refs mid-batch.
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
        if let Some(batch) = self.batch.take() {
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
        if let Some(batch) = self.batch.take() {
            FilesRefStore::release_locks(&batch);
        }
        self.store.invalidate_packed_cache_after_write();
    }
}

fn loose_storage_path_to_refname(storage_root: &Path, loose: &Path) -> String {
    loose
        .strip_prefix(storage_root)
        .map(|rel| rel.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|_| loose.to_string_lossy().into_owned())
}

fn first_loose_ref_file_under(dir: &Path) -> Option<PathBuf> {
    let entries = fs::read_dir(dir).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        let file_type = entry.file_type().ok()?;
        if file_type.is_dir() {
            if let Some(found) = first_loose_ref_file_under(&path) {
                return Some(found);
            }
        } else if file_type.is_file() && path.extension().is_none_or(|e| e != "lock") {
            return Some(path);
        }
    }
    None
}

fn name_unavailable_if_lock_path_io_blocked(
    common_dir: &Path,
    refname: &str,
    ref_path: &Path,
    lock: &Path,
    err: &Error,
) -> Option<RefnameUnavailable> {
    let source = match err {
        Error::GitDirPathIo { source, .. } => source,
        Error::Io(source) => source,
        _ => return None,
    };
    if lock.extension().is_none_or(|e| e != "lock") {
        return None;
    }
    let ancestor_path = ref_path.parent().filter(|p| p.is_file());
    let lock_parent = lock.parent().filter(|p| p.is_file());
    if let Some(blocking) = ancestor_path.or(lock_parent) {
        return Some(RefnameUnavailable::AncestorExists {
            blocking: loose_storage_path_to_refname(common_dir, blocking),
            new_ref: refname.to_owned(),
        });
    }
    match source.kind() {
        io::ErrorKind::AlreadyExists
        | io::ErrorKind::NotADirectory
        | io::ErrorKind::IsADirectory => {
            if let Some((blocking, _)) = refname.rsplit_once('/') {
                if blocking != "refs" {
                    return Some(RefnameUnavailable::AncestorExists {
                        blocking: blocking.to_owned(),
                        new_ref: refname.to_owned(),
                    });
                }
            }
            None
        }
        _ => None,
    }
}

fn name_unavailable_after_loose_rename_conflict(
    refname: &str,
    ref_path: &Path,
    storage_root: &Path,
) -> RefnameUnavailable {
    if ref_path.is_dir() {
        if let Some(descendant) = first_loose_ref_file_under(ref_path) {
            return RefnameUnavailable::DescendantExists {
                blocking: loose_storage_path_to_refname(storage_root, &descendant),
                new_ref: refname.to_owned(),
            };
        }
    }
    if let Some(parent) = ref_path.parent() {
        if parent.is_file() {
            return RefnameUnavailable::AncestorExists {
                blocking: loose_storage_path_to_refname(storage_root, parent),
                new_ref: refname.to_owned(),
            };
        }
    }
    RefnameUnavailable::DescendantExists {
        blocking: format!("{refname}/…"),
        new_ref: refname.to_owned(),
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
        // The directory entry's type saves a `stat` and a `readlink` per ref; symlinks and
        // unknown types still go through `stat`, which follows symlinks.
        let file_type = entry.file_type().ok();
        if file_type.is_some_and(|t| t.is_file()) {
            if wanted {
                if let Ok(r) = crate::refs::read_regular_ref_file(&path) {
                    out.insert(refname, ref_to_raw(r));
                }
            }
            continue;
        }
        if file_type.is_some_and(|t| t.is_dir()) {
            collect_loose_raw_refs(&path, &format!("{refname}/"), want, out)?;
            continue;
        }
        let meta = match fs::metadata(&path) {
            Ok(m) => m,
            // Concurrent delete or an unreadable entry (e.g. dangling symlink): skip it.
            Err(_) => continue,
        };
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

/// The line `entry` is written as in a reflog file.
fn reflog_line(entry: &ReflogEntry) -> String {
    let (old_hex, new_hex) = crate::refs::reflog_oid_hex_pair(&entry.old_oid, &entry.new_oid);
    if entry.message.is_empty() {
        format!("{old_hex} {new_hex} {}\n", entry.identity)
    } else {
        format!(
            "{old_hex} {new_hex} {}\t{}\n",
            entry.identity, entry.message
        )
    }
}

/// The entries of the reflog file at `path` (none when it is missing). A message that is
/// not UTF-8, which Git writes for a commit subject in a legacy encoding, is read with
/// replacement characters rather than failing the read.
fn read_reflog_at_path(path: &Path) -> Result<Vec<ReflogEntry>> {
    let content = match fs::read(path) {
        Ok(bytes) => bytes,
        // A ref being created can collide with a file or directory in `logs/`; that is a
        // missing log here, and the ref conflict check reports the collision.
        Err(e) if crate::refs::io_error_is_missing_file(&e, path) => return Ok(Vec::new()),
        Err(e) => return Err(Error::Io(e)),
    };
    let text = utf8_or_lossy(&content);
    Ok(text
        .lines()
        .filter_map(reflog::parse_reflog_line_for_store)
        .collect())
}

/// `bytes` as text, validating on the fast path and replacing invalid sequences only when
/// there are any (`String::from_utf8_lossy` alone is markedly slower on large logs).
fn utf8_or_lossy(bytes: &[u8]) -> Cow<'_, str> {
    match std::str::from_utf8(bytes) {
        Ok(text) => Cow::Borrowed(text),
        Err(_) => String::from_utf8_lossy(bytes),
    }
}

fn changed_ref_names<'a>(
    before: &'a BTreeMap<String, RawRef>,
    after: &'a BTreeMap<String, RawRef>,
) -> BTreeSet<&'a String> {
    before
        .keys()
        .chain(after.keys())
        .filter(|name| before.get(*name) != after.get(*name))
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
                refs.verify_create_available(
                    name,
                    &extras.iter().map(String::as_str).collect(),
                    None
                ),
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
    fn a_ref_whose_reflog_is_not_utf8_moves_and_its_log_keeps_what_it_had() {
        let dir = git_dir();
        let path = dir.path();
        let writer = store(path);
        let name = "refs/heads/main";
        set(&writer, name, oid(1), Expected::Missing).expect("create");
        // Git logs a commit subject in a legacy encoding as it is, and the
        // store keeps a line it can't parse.
        let log = path.join("logs/refs/heads/main");
        fs::create_dir_all(log.parent().expect("parent")).expect("logs");
        let mut written = format!(
            "{} {} A <a@e> 1700000000 +0000\tcommit: Caf",
            oid(0).to_hex(),
            oid(1).to_hex()
        )
        .into_bytes();
        written.extend_from_slice(b"\xe9 latin-1\nnot a reflog line\n");
        fs::write(&log, &written).expect("log");

        let txn = RefTransaction::new()
            .update(RefUpdate {
                name: name.to_owned(),
                new_value: Some(RawRef::Direct(oid(2))),
                expected: Expected::Oid(oid(1)),
                reflog: Some(super::super::ReflogUpdate {
                    identity: "T <t@t> 1700000001 +0000".into(),
                    message: "moved".into(),
                    time: time::OffsetDateTime::UNIX_EPOCH,
                    old_oid: None,
                    new_oid: None,
                }),
                flags: RefUpdateFlags::default(),
            })
            .expect("txn");
        writer
            .prepare(txn)
            .expect("prepare")
            .commit()
            .expect("commit");

        assert_eq!(
            writer.read_raw(name).expect("read"),
            Some(RawRef::Direct(oid(2)))
        );
        let bytes = fs::read(&log).expect("log");
        assert!(
            bytes.starts_with(&written),
            "what the log had stays as written"
        );
        assert_eq!(
            String::from_utf8_lossy(&bytes[written.len()..]),
            format!(
                "{} {} T <t@t> 1700000001 +0000\tmoved\n",
                oid(1).to_hex(),
                oid(2).to_hex()
            )
        );
        let mut messages = Vec::new();
        writer
            .for_each_reflog_entry(name, false, &mut |entry| {
                messages.push(entry.message.clone());
                ControlFlow::Continue(())
            })
            .expect("read log");
        assert_eq!(messages, ["commit: Caf\u{fffd} latin-1", "moved"]);
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

    fn create_with_reflog(name: &str, value: ObjectId) -> RefTransaction {
        RefTransaction::new()
            .update(RefUpdate {
                name: name.to_owned(),
                new_value: Some(RawRef::Direct(value)),
                expected: Expected::Missing,
                reflog: Some(super::super::ReflogUpdate {
                    identity: "T <t@t>".into(),
                    message: "branch: Created from main".into(),
                    time: time::OffsetDateTime::UNIX_EPOCH,
                    old_oid: None,
                    new_oid: None,
                }),
                flags: RefUpdateFlags::default(),
            })
            .expect("txn")
    }

    /// Prepare a reflogged create of `refs/heads/rel` and check it names `blocking`, leaving
    /// no lock, ref or log behind.
    fn assert_rel_create_blocked_by(path: &Path, blocking_ref: &str) {
        let writer = store(path);
        let err = writer
            .prepare(create_with_reflog("refs/heads/rel", oid(2)))
            .map(|_| ())
            .expect_err("conflict");
        assert!(
            matches!(
                err,
                RefStoreError::NameUnavailable {
                    reason: RefnameUnavailable::DescendantExists { ref blocking, ref new_ref }
                } if blocking == blocking_ref && new_ref == "refs/heads/rel"
            ),
            "{err:?}"
        );
        assert_eq!(writer.read_raw("refs/heads/rel").expect("read"), None);
        assert!(!path.join("refs/heads/rel.lock").exists());
        assert!(!path.join("logs/refs/heads/rel").is_file());
    }

    #[test]
    fn create_with_reflog_over_a_loose_descendant_names_it() {
        let dir = git_dir();
        let path = dir.path();
        store(path)
            .prepare(create_with_reflog("refs/heads/rel/1.0", oid(1)))
            .expect("prepare seed")
            .commit()
            .expect("seed");
        assert!(path.join("logs/refs/heads/rel/1.0").is_file());
        assert_rel_create_blocked_by(path, "refs/heads/rel/1.0");
    }

    #[test]
    fn create_with_reflog_over_a_packed_descendant_with_its_log_dir_names_it() {
        let dir = git_dir();
        let path = dir.path();
        // `pack-refs` leaves `logs/refs/heads/rel/1.0`, and an empty `refs/heads/rel/`.
        fs::write(
            path.join("packed-refs"),
            format!(
                "# pack-refs with: peeled fully-peeled sorted \n{} refs/heads/rel/1.0\n",
                oid(1)
            ),
        )
        .expect("packed-refs");
        fs::create_dir_all(path.join("refs/heads/rel")).expect("empty ref dir");
        fs::create_dir_all(path.join("logs/refs/heads/rel")).expect("logs");
        fs::write(path.join("logs/refs/heads/rel/1.0"), "").expect("log");
        assert_rel_create_blocked_by(path, "refs/heads/rel/1.0");
    }

    #[test]
    fn create_with_reflog_over_a_stray_log_directory_names_the_log() {
        let dir = git_dir();
        let path = dir.path();
        fs::create_dir_all(path.join("logs/refs/heads/rel/deep")).expect("logs");
        fs::write(path.join("logs/refs/heads/rel/deep/2.0"), "").expect("log");
        assert_rel_create_blocked_by(path, "refs/heads/rel/deep/2.0");
        assert!(path.join("logs/refs/heads/rel/deep/2.0").is_file());
    }

    #[test]
    fn create_with_reflog_removes_an_empty_log_directory() {
        let dir = git_dir();
        let path = dir.path();
        fs::create_dir_all(path.join("logs/refs/heads/rel/a/b")).expect("empty logs");
        let writer = store(path);
        writer
            .prepare(create_with_reflog("refs/heads/rel", oid(2)))
            .expect("prepare")
            .commit()
            .expect("commit");
        assert_eq!(
            writer.read_raw("refs/heads/rel").expect("read"),
            Some(RawRef::Direct(oid(2)))
        );
        assert_eq!(
            writer
                .read_reflog_entries("refs/heads/rel")
                .expect("log")
                .len(),
            1
        );
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
            path.join("refs/remotes").is_dir(),
            "refs/remotes/ namespace kept"
        );
        assert!(
            !path.join("refs/remotes/o").exists(),
            "emptied child dirs are pruned"
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

    /// Refs deleted while listing must not fail the walk (matches Git upload-pack behavior).
    #[test]
    fn concurrent_loose_ref_delete_during_list_is_tolerated() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;
        use std::thread;
        use std::time::Duration;

        let dir = git_dir();
        set(
            &store(dir.path()),
            "refs/heads/main",
            oid(1),
            Expected::Missing,
        )
        .expect("seed main");

        let stop = Arc::new(AtomicBool::new(false));
        let writer_dir = dir.path().to_path_buf();
        let writer_stop = stop.clone();
        let writer = thread::spawn(move || {
            use crate::refs::{delete_ref, write_ref};
            let c = oid(2);
            while !writer_stop.load(Ordering::Relaxed) {
                for i in 0..7 {
                    let name = format!("refs/heads/d{i}/sub/x");
                    let _ = write_ref(&writer_dir, &name, &c);
                    let _ = delete_ref(&writer_dir, &name);
                }
            }
        });

        let lister = store(dir.path());
        let mut errors = 0usize;
        for _ in 0..400 {
            if lister
                .for_each_ref("", &mut |_| ControlFlow::Continue(()))
                .is_err()
            {
                errors += 1;
            }
        }
        stop.store(true, Ordering::Relaxed);
        writer.join().expect("writer");
        thread::sleep(Duration::from_millis(10));
        assert_eq!(
            errors, 0,
            "listing must tolerate refs deleted between readdir and open"
        );
    }

    fn create(name: &str, value: ObjectId) -> RefUpdate {
        RefUpdate {
            name: name.to_owned(),
            new_value: Some(RawRef::Direct(value)),
            expected: Expected::Missing,
            reflog: None,
            flags: RefUpdateFlags::default(),
        }
    }

    fn commit_initial(store: &FilesRefStore, updates: Vec<RefUpdate>) -> StoreResult<()> {
        store
            .prepare(RefTransaction::from_updates(updates)?.initial())?
            .commit()
    }

    fn loose_files_under(dir: &Path) -> BTreeSet<String> {
        let mut names = BTreeSet::new();
        collect_loose_file_names(dir, "", &mut names);
        names
    }

    #[test]
    fn initial_transaction_writes_one_sorted_packed_refs_and_no_loose_files() {
        let dir = git_dir();
        fs::write(
            dir.path().join("packed-refs"),
            format!(
                "# pack-refs with: peeled fully-peeled sorted \n{} refs/tags/kept\n",
                oid(7)
            ),
        )
        .expect("packed-refs");
        let store = store(dir.path());
        let mut updates: Vec<RefUpdate> = (0..500)
            .rev()
            .map(|i| create(&format!("refs/remotes/origin/b/n{i}"), oid(1)))
            .collect();
        updates.push(create("refs/tags/v1", oid(2)));
        commit_initial(&store, updates).expect("initial batch");

        assert!(loose_files_under(&dir.path().join("refs")).is_empty());
        assert!(!dir.path().join("packed-refs.lock").exists());
        let packed = fs::read_to_string(dir.path().join("packed-refs")).expect("read");
        let names: Vec<&str> = packed
            .lines()
            .skip(1)
            .map(|line| line.split_once(' ').expect("record").1)
            .collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        assert_eq!(names, sorted);
        assert_eq!(names.len(), 502);
        assert!(names.contains(&"refs/tags/kept"));
        assert_eq!(
            store.read_raw("refs/remotes/origin/b/n42").expect("read"),
            Some(RawRef::Direct(oid(1)))
        );
        assert_eq!(
            listed_names(&store, "refs/tags/"),
            ["refs/tags/kept", "refs/tags/v1"]
        );
    }

    #[test]
    fn initial_transaction_falls_back_to_loose_refs_when_it_cannot_pack() {
        let dir = git_dir();
        let store = store(dir.path());
        set(&store, "refs/heads/main", oid(1), Expected::Missing).expect("seed");

        // An update of an existing ref is not a create.
        let updates = vec![
            create("refs/heads/topic", oid(2)),
            RefUpdate {
                expected: Expected::Any,
                ..create("refs/heads/main", oid(3))
            },
        ];
        commit_initial(&store, updates).expect("regular batch");
        assert_eq!(
            loose_files_under(&dir.path().join("refs/heads")),
            BTreeSet::from(["/main".to_owned(), "/topic".to_owned()])
        );

        // A symbolic ref is not a plain create either.
        let symref = RefUpdate {
            new_value: Some(RawRef::Symbolic("refs/heads/main".to_owned())),
            ..create("refs/remotes/origin/HEAD", oid(1))
        };
        commit_initial(&store, vec![create("refs/tags/t", oid(4)), symref]).expect("symref");
        assert!(dir.path().join("refs/remotes/origin/HEAD").is_file());
        assert!(dir.path().join("refs/tags/t").is_file());
        assert!(!dir.path().join("packed-refs").exists());
    }

    #[test]
    fn initial_transaction_reports_name_conflicts() {
        let dir = git_dir();
        let store = store(dir.path());
        set(&store, "refs/heads/a", oid(1), Expected::Missing).expect("seed");

        let err = commit_initial(&store, vec![create("refs/heads/a/b", oid(2))])
            .expect_err("stored ancestor");
        assert!(matches!(
            err,
            RefStoreError::NameUnavailable {
                reason: RefnameUnavailable::AncestorExists { .. }
            }
        ));
        let err = commit_initial(
            &store,
            vec![
                create("refs/heads/x", oid(2)),
                create("refs/heads/x/y", oid(2)),
            ],
        )
        .expect_err("batch conflicts with itself");
        assert!(matches!(
            err,
            RefStoreError::NameUnavailable {
                reason: RefnameUnavailable::SameBatch { .. }
            }
        ));
        assert!(!dir.path().join("packed-refs").exists());
        assert!(!dir.path().join("packed-refs.lock").exists());
    }

    #[test]
    fn batch_blocked_by_a_file_in_place_of_a_parent_writes_nothing() {
        let dir = git_dir();
        fs::write(dir.path().join("refs/heads/block"), "not a ref\n").expect("obstruction");
        let store = store(dir.path());

        let err = commit_all(
            &store,
            vec![
                create("refs/heads/first", oid(1)),
                create("refs/heads/block/child", oid(2)),
            ],
        )
        .expect_err("refs/heads/block/child cannot be created");
        assert!(
            !matches!(err, RefStoreError::ExpectedMismatch { .. }),
            "{err:?}"
        );
        assert!(
            !dir.path().join("refs/heads/first").exists(),
            "the batch is all-or-nothing: no ref before the failing one is written"
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("refs/heads/block")).expect("read"),
            "not a ref\n"
        );
        assert!(!dir.path().join("refs/heads/first.lock").exists());
    }

    #[cfg(windows)]
    #[test]
    fn win32_initial_packed_transaction_accepts_reserved_ref_name() {
        let dir = git_dir();
        let store = store(dir.path());
        commit_initial(&store, vec![create("refs/heads/aux", oid(1))]).expect("packed initial");

        assert!(
            !dir.path().join("refs/heads/aux").exists(),
            "packed-only initial must not write a loose ref"
        );
        let packed = fs::read_to_string(dir.path().join("packed-refs")).expect("packed-refs");
        assert!(
            packed.contains("refs/heads/aux"),
            "reserved name may live in packed-refs: {packed}"
        );
    }

    #[cfg(windows)]
    #[test]
    fn win32_initial_loose_fallback_rejects_reserved_ref_name() {
        let dir = git_dir();
        let store = store(dir.path());
        set(&store, "refs/heads/main", oid(1), Expected::Missing).expect("seed");

        let err = commit_initial(
            &store,
            vec![
                create("refs/heads/aux", oid(2)),
                RefUpdate {
                    expected: Expected::Any,
                    ..create("refs/heads/main", oid(3))
                },
            ],
        )
        .expect_err("loose fallback");
        assert!(
            matches!(err, RefStoreError::InvalidRefName { .. }),
            "{err:?}"
        );
        assert!(!dir.path().join("refs/heads/aux").exists());
    }

    #[cfg(windows)]
    #[test]
    fn win32_rejects_reserved_ref_path_components() {
        let dir = git_dir();
        let store = store(dir.path());
        let oid = oid(9);

        for name in [
            "refs/heads/aux",
            "refs/heads/foo/com1",
            "refs/heads/COM1.txt",
        ] {
            let err = set(&store, name, oid, Expected::Missing).expect_err(name);
            assert!(
                matches!(err, RefStoreError::InvalidRefName { .. }),
                "{name}: {err:?}"
            );
            let loose = dir.path().join(name);
            assert!(!loose.exists(), "{name} must not be written");
            assert!(
                !dir.path().join(format!("{name}.lock")).exists(),
                "lock must not be created for {name}"
            );
            assert!(
                !dir.path().join("logs").join(name).exists(),
                "reflog must not be written for {name}"
            );
        }
    }

    #[cfg(windows)]
    #[test]
    fn win32_reftable_still_accepts_reserved_ref_names() {
        use super::reftable::ReftableRefStore;

        let store = ReftableRefStore::open_ephemeral().expect("reftable");
        let txn = RefTransaction::new()
            .update(RefUpdate {
                name: "refs/heads/aux".to_owned(),
                new_value: Some(RawRef::Direct(oid(1))),
                expected: Expected::Missing,
                reflog: None,
                flags: RefUpdateFlags::default(),
            })
            .expect("txn");
        store.prepare(txn).expect("prepare").commit().expect("aux");
    }

    #[cfg(windows)]
    #[test]
    fn win32_can_delete_preseeded_reserved_loose_ref() {
        let dir = git_dir();
        let hex = oid(3).to_hex();
        let aux = dir.path().join("refs/heads/aux");
        fs::write(&aux, format!("{hex}\n")).expect("seed aux");
        let store = store(dir.path());

        let txn = RefTransaction::new()
            .update(RefUpdate {
                name: "refs/heads/aux".to_owned(),
                new_value: None,
                expected: Expected::Any,
                reflog: None,
                flags: RefUpdateFlags::default(),
            })
            .expect("txn");
        store
            .prepare(txn)
            .expect("prepare")
            .commit()
            .expect("delete");
        assert!(!aux.exists());
    }
}
