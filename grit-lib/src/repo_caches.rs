//! Repository-scoped caches shared by an open [`Repository`].
//!
//! Config cascade, gitattributes stacks, filter-process registries, precompose flags, and
//! reftable-backend detection live here instead of process-global statics so concurrent
//! repositories on different threads do not cross-contaminate.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

fn canonical_config_path(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

/// Read repository-local `config` from disk (no caching; standalone [`ConfigSet::load`] paths use this).
pub(crate) fn read_local_config_file(path: &Path) -> Result<String> {
    std::fs::read_to_string(path).map_err(Error::Io)
}

use crate::commit_graph_file::CommitGraphChain;

use crate::attributes::{
    attr_file_stamp, attr_stamps_valid, collect_stack_stamps, load_gitattributes_bare_uncached,
    load_gitattributes_stack_uncached, AttrStackCacheEntry, ParsedGitAttributes,
};
use crate::blame::PromisorHydrateHook;
use crate::config::{
    config_cache_lookup, config_cache_store, config_file_stamps, ConfigCacheKey, ConfigSet,
    LoadConfigOptions,
};
use crate::diagnostics::{DiagnosticsHandle, NullDiagnostics, Warning};
use crate::environment::Environment;
use crate::error::{Error, Result};
use crate::filter_process::FilterProcessState;
use crate::objects::ObjectId;
use crate::pack_bitmap::BitmapIndexCache;
use crate::precompose_config::{
    effective_core_precomposeunicode_with_config, filesystem_nfd_nfc_aliases,
};
use crate::ref_storage::RefStorageFormat;
use crate::repo::Repository;

/// Parents and committer timestamps reused across rev-parse and revwalk on one handle.
#[derive(Default)]
pub(crate) struct CommitLinkageCache {
    pub parents: HashMap<ObjectId, Vec<ObjectId>>,
    pub times: HashMap<ObjectId, i64>,
}

/// Per-repository cache arena (held behind [`Arc`] on [`Repository`]).
pub struct RepoCaches {
    config_cache: Mutex<HashMap<ConfigCacheKey, crate::config::ConfigCacheEntry>>,
    /// Installed by [`crate::repo::Repository::refresh_config_session`]; hot paths read this
    /// without re-statting the config cascade.
    session_config: Mutex<Option<(ConfigCacheKey, Arc<ConfigSet>)>>,
    /// Parsed `[core]` / `[extensions]` from repository-local `config` (one disk read per handle).
    local_repo_format: Mutex<Option<crate::repo::RepositoryFormat>>,
    /// Raw bytes of repository-local `config` for helpers that would otherwise re-open the file.
    local_config_text: Mutex<Option<String>>,
    /// Canonical path paired with [`Self::local_config_text`] (MIDX and ref backend share one read).
    local_config_source_path: Mutex<Option<PathBuf>>,
    attr_stack: Mutex<HashMap<(PathBuf, PathBuf), AttrStackCacheEntry>>,
    attr_bare: Mutex<HashMap<PathBuf, AttrStackCacheEntry>>,
    attr_tree: Mutex<HashMap<ObjectId, Arc<ParsedGitAttributes>>>,
    pathspec_precompose: OnceLock<bool>,
    ref_storage_format: Mutex<HashMap<PathBuf, RefStorageFormat>>,
    pub(crate) ref_stores: Mutex<HashMap<PathBuf, Arc<dyn crate::refs::store::RefStore>>>,
    filters: FilterProcessState,
    promisor_hydrate: Mutex<Option<PromisorHydrateHook>>,
    bare_worktree_warn_seen: Mutex<HashSet<String>>,
    commit_graph_warn_seen: Mutex<HashSet<String>>,
    commit_graph: Mutex<Option<Arc<CommitGraphChain>>>,
    commit_linkage: Mutex<CommitLinkageCache>,
    bitmap_index: BitmapIndexCache,
    diagnostics: Mutex<DiagnosticsHandle>,
}

impl std::fmt::Debug for RepoCaches {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RepoCaches").finish_non_exhaustive()
    }
}

impl RepoCaches {
    /// Create an empty cache arena for one repository handle.
    #[must_use]
    pub fn new(command_runner: Arc<dyn crate::command_runner::CommandRunner>) -> Arc<Self> {
        Self::with_diagnostics(command_runner, Arc::new(NullDiagnostics))
    }

    /// Create a cache arena wired to the given diagnostic sink.
    #[must_use]
    pub fn with_diagnostics(
        command_runner: Arc<dyn crate::command_runner::CommandRunner>,
        diagnostics: DiagnosticsHandle,
    ) -> Arc<Self> {
        Arc::new(Self {
            config_cache: Mutex::new(HashMap::new()),
            session_config: Mutex::new(None),
            local_repo_format: Mutex::new(None),
            local_config_text: Mutex::new(None),
            local_config_source_path: Mutex::new(None),
            attr_stack: Mutex::new(HashMap::new()),
            attr_bare: Mutex::new(HashMap::new()),
            attr_tree: Mutex::new(HashMap::new()),
            pathspec_precompose: OnceLock::new(),
            ref_storage_format: Mutex::new(HashMap::new()),
            ref_stores: Mutex::new(HashMap::new()),
            filters: FilterProcessState::new(command_runner),
            promisor_hydrate: Mutex::new(None),
            bare_worktree_warn_seen: Mutex::new(HashSet::new()),
            commit_graph_warn_seen: Mutex::new(HashSet::new()),
            commit_graph: Mutex::new(None),
            commit_linkage: Mutex::new(CommitLinkageCache::default()),
            bitmap_index: BitmapIndexCache::default(),
            diagnostics: Mutex::new(diagnostics),
        })
    }

    /// Cached [`crate::pack_bitmap::BitmapIndex`] for this repository handle.
    pub(crate) fn bitmap_index(&self) -> &BitmapIndexCache {
        &self.bitmap_index
    }

    /// Shared commit parent/timestamp cache for merge-base and revwalk.
    pub(crate) fn commit_linkage(&self) -> &Mutex<CommitLinkageCache> {
        &self.commit_linkage
    }

    /// Return the diagnostic sink for this cache arena.
    pub(crate) fn diagnostics_handle(&self) -> DiagnosticsHandle {
        self.diagnostics
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Replace the diagnostic sink (kept in sync with [`Repository::set_diagnostics`]).
    pub(crate) fn set_diagnostics(&self, sink: DiagnosticsHandle) {
        *self.diagnostics.lock().unwrap_or_else(|e| e.into_inner()) = sink;
    }

    /// Emit `core.bare` / `core.worktree` conflict warning at most once per git dir on this repo.
    pub fn warn_bare_worktree_conflict_once(&self, git_dir: &Path) {
        let key = git_dir
            .canonicalize()
            .unwrap_or_else(|_| git_dir.to_path_buf())
            .to_string_lossy()
            .to_string();
        let Ok(mut guard) = self.bare_worktree_warn_seen.lock() else {
            return;
        };
        if guard.insert(key) {
            self.diagnostics_handle()
                .warn(Warning::CoreBareWithWorktree);
        }
    }

    /// Return `true` when this warning id has not been emitted yet for this repository handle.
    pub fn should_warn_commit_graph_once(&self, id: &str) -> bool {
        let Ok(mut guard) = self.commit_graph_warn_seen.lock() else {
            return true;
        };
        guard.insert(id.to_string())
    }

    /// Parsed commit-graph chain for this repository handle (at most one disk load).
    pub(crate) fn commit_graph_chain(&self, objects_dir: &Path) -> Option<Arc<CommitGraphChain>> {
        let Ok(mut guard) = self.commit_graph.lock() else {
            return None;
        };
        if let Some(chain) = guard.as_ref() {
            return Some(Arc::clone(chain));
        }
        let chain = CommitGraphChain::try_load_with_caches(objects_dir, Some(self))
            .ok()
            .flatten()?;
        let arc = Arc::new(chain);
        *guard = Some(Arc::clone(&arc));
        Some(arc)
    }

    /// Drop a cached commit-graph after the on-disk file was rewritten.
    pub(crate) fn invalidate_commit_graph_cache(&self) {
        if let Ok(mut guard) = self.commit_graph.lock() {
            *guard = None;
        }
    }

    /// Load the standard configuration cascade with repository-scoped memoization.
    ///
    /// # Errors
    ///
    /// Propagates config parse and I/O errors from the uncached loader.
    pub fn load_config(
        &self,
        env: &Environment,
        git_dir: Option<&Path>,
        include_system: bool,
    ) -> Result<Arc<ConfigSet>> {
        let opts = LoadConfigOptions {
            include_system,
            ..LoadConfigOptions::default()
        };
        self.load_config_with_options(env, git_dir, &opts)
    }

    /// Load a configuration cascade with explicit options and repository-scoped memoization.
    ///
    /// # Errors
    ///
    /// Propagates config parse and I/O errors from the uncached loader.
    pub fn load_config_with_options(
        &self,
        env: &Environment,
        git_dir: Option<&Path>,
        opts: &LoadConfigOptions,
    ) -> Result<Arc<ConfigSet>> {
        let mut effective = opts.clone();
        if effective.diagnostics.is_none() {
            effective.diagnostics = Some(self.diagnostics_handle());
        }
        let opts = &effective;
        let key = ConfigCacheKey::new(git_dir, opts);
        if let Ok(guard) = self.session_config.lock() {
            if let Some((_, cfg)) = guard.as_ref() {
                return Ok(Arc::clone(cfg));
            }
        }
        let Some(env_fp) = env.config_fingerprint() else {
            #[cfg(test)]
            crate::config::cascade_load_counters::record_uncached();
            let set = ConfigSet::load_with_options_uncached(env, git_dir, opts, &mut Vec::new())?;
            return Ok(Arc::new(set));
        };
        let base_stamps = config_file_stamps(env, git_dir, opts);
        {
            let cache = self
                .config_cache
                .lock()
                .map_err(|e| Error::Message(format!("config cache lock poisoned: {e}")))?;
            if let Some(cached) = config_cache_lookup(&cache, &key, &env_fp, &base_stamps) {
                return Ok(Arc::new(cached));
            }
        }
        #[cfg(test)]
        crate::config::cascade_load_counters::record_uncached();
        let mut included_files = Vec::new();
        let set = ConfigSet::load_with_options_uncached(env, git_dir, opts, &mut included_files)?;
        included_files.sort_unstable();
        included_files.dedup();
        let extra_stamps = crate::config::stamp_paths(included_files);
        let mut cache = self
            .config_cache
            .lock()
            .map_err(|e| Error::Message(format!("config cache lock poisoned: {e}")))?;
        config_cache_store(
            &mut cache,
            key,
            env_fp,
            base_stamps,
            extra_stamps,
            Arc::new(set.clone()),
        );
        Ok(Arc::new(set))
    }

    /// Drop cached repository-local `config` body and derived parse (not the full cascade cache).
    pub(crate) fn clear_local_config_derived(&self) {
        if let Ok(mut slot) = self.local_repo_format.lock() {
            *slot = None;
        }
        if let Ok(mut slot) = self.local_config_text.lock() {
            *slot = None;
        }
        if let Ok(mut slot) = self.local_config_source_path.lock() {
            *slot = None;
        }
    }

    /// Cached local `config` body when `config_path` matches the path loaded by [`Self::local_config_text`].
    pub(crate) fn cached_local_config_text_for_path(&self, config_path: &Path) -> Option<String> {
        let key = canonical_config_path(config_path);
        let stored_path = self.local_config_source_path.lock().ok()?.clone()?;
        if canonical_config_path(&stored_path) != key {
            return None;
        }
        self.local_config_text.lock().ok()?.clone()
    }

    /// Drop derived state that depends on repository-local config text or cascade stamps.
    pub(crate) fn invalidate_config_file_derived(&self) {
        self.clear_local_config_derived();
        if let Ok(mut guard) = self.ref_stores.lock() {
            guard.clear();
        }
        if let Ok(mut guard) = self.ref_storage_format.lock() {
            guard.clear();
        }
        if let Ok(mut cache) = self.config_cache.lock() {
            cache.clear();
        }
    }

    /// Drop all memoized configuration cascades for this repository handle.
    pub fn invalidate_config_cache(&self) {
        self.clear_session_config();
        self.invalidate_config_file_derived();
    }

    /// Repository-local `config` body (linked worktree common dir when needed), read at most once per handle until invalidation.
    pub(crate) fn local_config_text(&self, config_path: &Path) -> Result<String> {
        if let Ok(guard) = self.local_config_text.lock() {
            if let Some(text) = guard.as_ref() {
                return Ok(text.clone());
            }
        }
        let text = read_local_config_file(config_path)?;
        let source = canonical_config_path(config_path);
        if let Ok(mut guard) = self.local_config_text.lock() {
            *guard = Some(text.clone());
        }
        if let Ok(mut guard) = self.local_config_source_path.lock() {
            *guard = Some(source);
        }
        Ok(text)
    }

    /// Parsed repository format from local `config`, cached for ref backend detection and validation.
    pub(crate) fn repository_format(
        &self,
        _git_dir: &Path,
        config_path: &Path,
    ) -> Result<crate::repo::RepositoryFormat> {
        if let Ok(guard) = self.local_repo_format.lock() {
            if let Some(parsed) = guard.as_ref() {
                return Ok(parsed.clone());
            }
        }
        let content = self.local_config_text(config_path)?;
        let parsed = crate::repo::parse_repository_format(&content, config_path)?;
        if let Ok(mut guard) = self.local_repo_format.lock() {
            *guard = Some(parsed.clone());
        }
        Ok(parsed)
    }

    /// Install or replace the session config snapshot (see [`crate::repo::Repository`] rustdoc).
    pub(crate) fn set_session_config(&self, key: ConfigCacheKey, config: Arc<ConfigSet>) {
        if let Ok(mut slot) = self.session_config.lock() {
            *slot = Some((key, config));
        }
    }

    /// Drop the session config snapshot (part of [`Self::invalidate_config_cache`]).
    pub(crate) fn clear_session_config(&self) {
        if let Ok(mut slot) = self.session_config.lock() {
            *slot = None;
        }
    }

    /// Stamp metadata for a cached cascade entry (after [`Self::load_config_with_options`]).
    pub(crate) fn config_entry_stamps(
        &self,
        key: &ConfigCacheKey,
    ) -> Option<crate::config::ConfigCacheEntryStamps> {
        let cache = self.config_cache.lock().ok()?;
        let entry = cache.get(key)?;
        Some((
            entry.env_fingerprint.clone(),
            entry.base_stamps.clone(),
            entry.extra_stamps.clone(),
        ))
    }

    /// Evict config cache entries whose stamp lists mention `path` (in-process config writes).
    pub fn evict_config_for_path(&self, path: &Path) {
        if let Ok(mut cache) = self.config_cache.lock() {
            cache.retain(|_, entry| {
                !entry
                    .base_stamps
                    .iter()
                    .chain(&entry.extra_stamps)
                    .any(|(p, _)| p == path)
            });
        }
        if let Ok(mut slot) = self.local_config_text.lock() {
            *slot = None;
        }
        if let Ok(mut slot) = self.local_config_source_path.lock() {
            *slot = None;
        }
    }

    /// Memoized work-tree gitattributes stack for this repository.
    ///
    /// # Errors
    ///
    /// Propagates attribute load and I/O errors.
    pub fn load_gitattributes_stack(
        &self,
        repo: &Repository,
        work_tree: &Path,
    ) -> Result<ParsedGitAttributes> {
        let key = (repo.git_dir().to_path_buf(), work_tree.to_path_buf());
        {
            let cache = self
                .attr_stack
                .lock()
                .map_err(|e| Error::Message(format!("attr stack cache lock poisoned: {e}")))?;
            if let Some(entry) = cache.get(&key) {
                if attr_stamps_valid(entry) {
                    return Ok((*entry.parsed).clone());
                }
            }
        }
        let (file_stamps, dir_stamps) = collect_stack_stamps(repo, work_tree)?;
        let parsed = load_gitattributes_stack_uncached(repo, work_tree)?;
        let mut cache = self
            .attr_stack
            .lock()
            .map_err(|e| Error::Message(format!("attr stack cache lock poisoned: {e}")))?;
        cache.insert(
            key,
            AttrStackCacheEntry {
                file_stamps,
                dir_stamps,
                parsed: Arc::new(parsed.clone()),
            },
        );
        Ok(parsed)
    }

    /// Memoized bare-repo gitattributes (info/attributes + global file).
    ///
    /// # Errors
    ///
    /// Propagates attribute load and I/O errors.
    pub fn load_gitattributes_bare(&self, repo: &Repository) -> Result<ParsedGitAttributes> {
        let key = repo.git_dir().to_path_buf();
        {
            let cache = self
                .attr_bare
                .lock()
                .map_err(|e| Error::Message(format!("attr bare cache lock poisoned: {e}")))?;
            if let Some(entry) = cache.get(&key) {
                if attr_stamps_valid(entry) {
                    return Ok((*entry.parsed).clone());
                }
            }
        }
        let mut file_stamps = Vec::new();
        if let Some(g) = crate::attributes::global_attributes_path(repo)? {
            file_stamps.push((g.clone(), attr_file_stamp(&g)));
        }
        let info = repo.git_dir().join("info/attributes");
        file_stamps.push((info.clone(), attr_file_stamp(&info)));
        let parsed = load_gitattributes_bare_uncached(repo)?;
        let mut cache = self
            .attr_bare
            .lock()
            .map_err(|e| Error::Message(format!("attr bare cache lock poisoned: {e}")))?;
        cache.insert(
            key,
            AttrStackCacheEntry {
                file_stamps,
                dir_stamps: Vec::new(),
                parsed: Arc::new(parsed.clone()),
            },
        );
        Ok(parsed)
    }

    /// Memoized gitattributes parsed from an immutable tree object.
    pub fn load_gitattributes_from_tree_cached(
        &self,
        odb: &crate::odb::Odb,
        tree_oid: &ObjectId,
        loader: impl FnOnce() -> Result<ParsedGitAttributes>,
    ) -> Result<ParsedGitAttributes> {
        let _ = odb;
        if let Ok(cache) = self.attr_tree.lock() {
            if let Some(parsed) = cache.get(tree_oid) {
                return Ok((**parsed).clone());
            }
        }
        let parsed = loader()?;
        if let Ok(mut cache) = self.attr_tree.lock() {
            cache.insert(*tree_oid, Arc::new(parsed.clone()));
        }
        Ok(parsed)
    }

    /// Whether pathspec comparisons should NFC-normalize paths for this repository.
    #[must_use]
    pub fn pathspec_precompose_enabled(&self, env: &Environment, git_dir: &Path) -> bool {
        *self.pathspec_precompose.get_or_init(|| {
            let cfg = self
                .load_config(env, Some(git_dir), true)
                .ok()
                .map(|arc| (*arc).clone());
            let enabled = cfg
                .as_ref()
                .map(|c| effective_core_precomposeunicode_with_config(Some(git_dir), Some(c)))
                .unwrap_or(false);
            enabled && filesystem_nfd_nfc_aliases(git_dir)
        })
    }

    /// Ref storage backend from repository-local config (one parse per handle until invalidation).
    ///
    /// # Errors
    ///
    /// Propagates local config I/O and format parse failures.
    pub fn ref_storage_format_from_local_config(&self, git_dir: &Path) -> Result<RefStorageFormat> {
        let Some(config_path) = crate::repo::repository_config_path_for_caches(git_dir) else {
            return Ok(RefStorageFormat::Files);
        };
        let parsed = self.repository_format(git_dir, &config_path)?;
        RefStorageFormat::from_repository_format(&parsed)
    }

    /// Cached ref storage format for `git_dir`.
    ///
    /// On detection failure, returns [`crate::RefStorageFormat::Files`].
    #[must_use]
    pub fn ref_storage_format(&self, git_dir: &Path) -> RefStorageFormat {
        let key = git_dir
            .canonicalize()
            .unwrap_or_else(|_| git_dir.to_path_buf());
        if let Ok(guard) = self.ref_storage_format.lock() {
            if let Some(v) = guard.get(&key) {
                return *v;
            }
        }
        let v =
            crate::ref_storage::detect_format(git_dir).unwrap_or(crate::RefStorageFormat::Files);
        if let Ok(mut guard) = self.ref_storage_format.lock() {
            guard.insert(key, v);
        }
        v
    }

    /// Open (or reuse) the [`crate::refs::store::RefStore`] for `git_dir`.
    ///
    /// # Errors
    ///
    /// Propagates backend detection and initialization failures.
    pub fn open_ref_store(
        &self,
        git_dir: &Path,
        env: &Environment,
    ) -> Result<Arc<dyn crate::refs::store::RefStore>> {
        let key = git_dir
            .canonicalize()
            .unwrap_or_else(|_| git_dir.to_path_buf());
        if let Ok(guard) = self.ref_stores.lock() {
            if let Some(store) = guard.get(&key) {
                return Ok(Arc::clone(store));
            }
        }
        let store = crate::refs::store::open_ref_store_uncached(git_dir, env, Some(self))?;
        if let Ok(mut guard) = self.ref_stores.lock() {
            guard.insert(key, Arc::clone(&store));
        }
        Ok(store)
    }

    /// Filter-process registry and disabled-driver set for this repository.
    #[must_use]
    pub fn filters(&self) -> &FilterProcessState {
        &self.filters
    }

    /// Optional promisor-object hydration hook for blame on partial clones.
    pub fn set_promisor_hydrate_hook(&self, hook: Option<PromisorHydrateHook>) {
        if let Ok(mut slot) = self.promisor_hydrate.lock() {
            *slot = hook;
        }
    }

    /// Return the promisor hydration hook installed on this repository, if any.
    #[must_use]
    pub fn promisor_hydrate_hook(&self) -> Option<PromisorHydrateHook> {
        self.promisor_hydrate.lock().ok().and_then(|g| *g)
    }
}

impl Drop for RepoCaches {
    fn drop(&mut self) {
        self.filters.shutdown_all();
    }
}
