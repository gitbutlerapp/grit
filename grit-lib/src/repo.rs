//! Repository discovery and the primary `Repository` handle.
//!
//! # Discovery
//!
//! [`Repository::discover`] walks up from a starting directory to find the
//! nearest `.git` directory (or bare repository), honouring `GIT_DIR` and
//! `GIT_WORK_TREE` environment variables and the `.git` gitfile indirection.
//!
//! # Structure
//!
//! A [`Repository`] owns:
//!
//! - `git_dir` — absolute path to the `.git` directory (or the repo root for
//!   bare repos).
//! - `work_tree` — `Some(path)` for non-bare repos, `None` for bare.
//! - [`Odb`] — the loose object database.

use std::collections::BTreeSet;
use std::fs;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use crate::command_runner::CommandRunner;
use crate::config::{ConfigFile, ConfigScope, ConfigSet, LoadConfigOptions};
use crate::diagnostics::{self, DiagnosticsHandle};
use crate::environment::{Environment, RepositoryOptions};
use crate::error::{Error, Result};
use crate::hooks::run_hook;
use crate::index::Index;
use crate::init_filesystem::{apply_init_filesystem_config, InitFilesystemConfigOptions};
use crate::objects::parse_commit;
use crate::odb::Odb;
use crate::repo_caches::RepoCaches;
use crate::rev_parse::is_inside_work_tree;
use crate::sparse_checkout::effective_cone_mode_for_sparse_file;
use crate::split_index::{write_index_file_split, WriteSplitIndexRequest};
use crate::state::resolve_head;
use crate::worktree_cwd::cwd_relative_under_work_tree;

/// Compute Git `GIT_PREFIX` for `env.cwd` relative to the work tree (POSIX, no trailing `/`).
fn compute_git_prefix(env: &Environment, work_tree: &Path) -> Option<String> {
    if let Some(prefix) = env.git_prefix.as_deref() {
        let p = prefix.trim();
        if !p.is_empty() {
            return Some(p.to_owned());
        }
    }
    let rel = cwd_relative_under_work_tree(work_tree, &env.cwd)?;
    if rel.is_empty() {
        None
    } else {
        Some(rel)
    }
}

fn read_sparse_checkout_patterns(git_dir: &Path) -> Vec<String> {
    let path = git_dir.join("info").join("sparse-checkout");
    let Ok(content) = fs::read_to_string(&path) else {
        return Vec::new();
    };
    content
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(String::from)
        .collect()
}

/// A handle to an open Git repository.
pub struct Repository {
    /// Absolute path to the git directory (`.git/` or bare repo root).
    pub git_dir: PathBuf,
    /// Absolute path to the working tree, or `None` for bare repos.
    pub work_tree: Option<PathBuf>,
    /// Loose object database.
    pub odb: Odb,
    /// Discovery provenance: true when opened via `GIT_DIR` env or explicit API.
    ///
    /// This suppresses safe.bareRepository implicit checks.
    pub explicit_git_dir: bool,
    /// When the repo was found by walking from a directory containing `.git` / a gitfile,
    /// that directory (matches Git's setup trace using `.git` for the default git-dir).
    pub discovery_root: Option<PathBuf>,
    /// `GIT_WORK_TREE` was set without `GIT_DIR` and applied after discovery (t1510 #1, #5, …).
    pub work_tree_from_env: bool,
    /// `.git` was a gitfile (not a directory) when the repo was discovered.
    pub discovery_via_gitfile: bool,
    /// Lazily loaded repository config snapshot (system / global / local / worktree cascade).
    ///
    /// Hot paths should call [`Repository::config`] once per operation and pass `&ConfigSet`
    /// down instead of calling [`ConfigSet::load`] repeatedly. [`Repository::reload_config`]
    /// replaces the snapshot after in-process config writes so subsequent operations see updates
    /// without reopening the repository.
    /// Repository-scoped caches (config, attributes, filters, precompose, …).
    caches: Arc<RepoCaches>,
    /// Pluggable ref backend selected for this repository.
    ref_store: Arc<dyn crate::refs::store::RefStore>,
    /// Discovery and configuration environment used to open this repository.
    environment: Arc<Environment>,
    /// Repository-relative path of [`Environment::cwd`] under [`Self::work_tree`] (Git `GIT_PREFIX`).
    git_prefix: Option<String>,
    /// Injectable subprocess runner (hooks, filters, helpers).
    command_runner: Arc<dyn CommandRunner>,
    diagnostics: DiagnosticsHandle,
    network_trace: bool,
    /// Embedder override for rev-parse relative dates (`@{yesterday}`, etc.).
    reference_unix_time: Option<i64>,
    test_assume_different_owner: bool,
    force_split_index: bool,
}

impl std::fmt::Debug for Repository {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Repository")
            .field("git_dir", &self.git_dir)
            .field("work_tree", &self.work_tree)
            .field("explicit_git_dir", &self.explicit_git_dir)
            .field("network_trace", &self.network_trace)
            .finish_non_exhaustive()
    }
}

/// Legacy alias kept for [`Odb`] wiring during the repository-cache migration.
pub(crate) type RepositoryConfigSnapshot = Arc<RepoCaches>;

/// Repository-level settings derived from config that are read on hot paths.
#[derive(Debug, Clone)]
struct RepoCachedSettings {
    /// `core.useReplaceRefs` (default `true`).
    use_replace_refs: bool,
    /// Effective `refs/replace/` base path (always slash-terminated).
    replace_ref_base: String,
}

impl Repository {
    fn config_load_options(&self, include_system: bool) -> LoadConfigOptions {
        LoadConfigOptions {
            include_system,
            include_ctx: crate::config::IncludeContext {
                git_dir: Some(self.git_dir.clone()),
                cwd: self.environment.cwd.clone(),
                pwd: self.environment.pwd.clone(),
                ..Default::default()
            },
            diagnostics: Some(self.diagnostics()),
            ..Default::default()
        }
    }

    fn from_canonical_git_dir(
        options: &RepositoryOptions,
        git_dir: PathBuf,
        work_tree: Option<&Path>,
    ) -> Result<Self> {
        let environment = Arc::new(options.environment.clone());
        let command_runner = Arc::clone(&options.command_runner);
        // Check HEAD exists or is a symlink (linked worktrees have a symlink HEAD)
        let head_path = git_dir.join("HEAD");
        if !head_path.exists() && !head_path.is_symlink() {
            return Err(Error::NotARepository(git_dir.display().to_string()));
        }

        // For git worktrees the `objects/` directory lives in the common git
        // directory pointed to by the `commondir` file.
        let objects_dir = if git_dir.join("objects").exists() {
            git_dir.join("objects")
        } else if let Some(common_dir) = resolve_common_dir(&git_dir) {
            common_dir.join("objects")
        } else {
            return Err(Error::NotARepository(git_dir.display().to_string()));
        };

        if !objects_dir.exists() {
            return Err(Error::NotARepository(git_dir.display().to_string()));
        }

        let work_tree = match work_tree {
            Some(p) => {
                let cwd = environment.discovery_cwd();
                let mut resolved = if p.is_absolute() {
                    p.to_path_buf()
                } else {
                    cwd.join(p)
                };
                if resolved.exists() {
                    resolved = resolved
                        .canonicalize()
                        .map_err(|_| Error::PathError(p.display().to_string()))?;
                }
                Some(resolved)
            }
            None => None,
        };

        let caches = RepoCaches::with_diagnostics(
            Arc::clone(&command_runner),
            Arc::clone(&options.diagnostics),
        );
        let odb = if let Some(ref wt) = work_tree {
            Odb::with_work_tree(&objects_dir, wt)
                .with_config_git_dir(git_dir.clone())
                .with_shared_config_state(caches.clone(), environment.clone())
        } else {
            Odb::new(&objects_dir)
                .with_config_git_dir(git_dir.clone())
                .with_shared_config_state(caches.clone(), environment.clone())
        };

        let git_prefix = work_tree
            .as_ref()
            .and_then(|wt| compute_git_prefix(environment.as_ref(), wt));

        let ref_store = caches.open_ref_store(&git_dir, environment.as_ref())?;

        Ok(Self {
            git_dir,
            work_tree,
            odb,
            explicit_git_dir: false,
            discovery_root: None,
            work_tree_from_env: false,
            discovery_via_gitfile: false,
            caches,
            ref_store,
            environment,
            git_prefix,
            command_runner,
            diagnostics: Arc::clone(&options.diagnostics),
            network_trace: options.network_trace,
            reference_unix_time: options.reference_unix_time,
            test_assume_different_owner: options.test_assume_different_owner,
            force_split_index: options.force_split_index,
        })
    }

    /// Return this handle's shared cache arena.
    #[must_use]
    pub(crate) fn caches(&self) -> &Arc<RepoCaches> {
        &self.caches
    }

    /// Install a promisor-object hydration hook for blame on partial clones.
    pub fn set_promisor_hydrate_hook(&self, hook: Option<crate::blame::PromisorHydrateHook>) {
        self.caches.set_promisor_hydrate_hook(hook);
    }

    /// Return the promisor hydration hook for this repository, if installed.
    #[must_use]
    pub fn promisor_hydrate_hook(&self) -> Option<crate::blame::PromisorHydrateHook> {
        self.caches.promisor_hydrate_hook()
    }

    /// Return the [`Environment`] used to discover or open this repository.
    #[must_use]
    pub fn environment(&self) -> &Environment {
        &self.environment
    }

    /// Unix timestamp for rev-parse relative date selectors (`@{yesterday}`, etc.).
    #[must_use]
    pub fn reference_unix_time(&self) -> i64 {
        self.wall_clock_epoch()
    }

    /// Wall clock used for commit dates, rerere GC, relative dates, and similar operations on this handle.
    #[must_use]
    pub fn wall_clock_epoch(&self) -> i64 {
        if let Some(ts) = self.reference_unix_time {
            return ts;
        }
        if let Some(ts) = self.environment.git_now_date_override() {
            return ts;
        }
        crate::git_date::tm::process_wall_clock_sec()
    }

    /// Subprocess runner used for hooks, filters, and helpers.
    #[must_use]
    pub fn command_runner(&self) -> Arc<dyn CommandRunner> {
        Arc::clone(&self.command_runner)
    }

    /// Repository-relative cwd prefix under the work tree (Git `GIT_PREFIX`), if any.
    #[must_use]
    pub fn git_prefix(&self) -> Option<&str> {
        self.git_prefix.as_deref()
    }

    fn repo_cached_settings_from_config(
        cfg: &ConfigSet,
        environment: &Environment,
    ) -> RepoCachedSettings {
        let use_replace_refs = cfg
            .get_bool("core.useReplaceRefs")
            .and_then(|r| r.ok())
            .unwrap_or(true);
        let replace_ref_base = environment
            .git_replace_ref_base
            .clone()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "refs/replace/".to_owned());
        let replace_ref_base = if replace_ref_base.ends_with('/') {
            replace_ref_base
        } else {
            format!("{replace_ref_base}/")
        };
        RepoCachedSettings {
            use_replace_refs,
            replace_ref_base,
        }
    }

    fn ensure_config_arc(&self) -> Result<Arc<ConfigSet>> {
        let opts = self.config_load_options(true);
        self.caches
            .load_config_with_options(self.environment.as_ref(), Some(&self.git_dir), &opts)
    }

    /// Return the diagnostic sink installed on this repository.
    #[must_use]
    pub fn diagnostics(&self) -> DiagnosticsHandle {
        Arc::clone(&self.diagnostics)
    }

    /// Replace the diagnostic sink (for embedders and the CLI).
    pub fn set_diagnostics(&mut self, sink: DiagnosticsHandle) {
        self.caches.set_diagnostics(Arc::clone(&sink));
        self.diagnostics = sink;
    }

    /// Whether network trace events should be delivered to the diagnostic sink.
    #[must_use]
    pub fn network_trace_enabled(&self) -> bool {
        self.network_trace
    }

    /// Enable or disable network trace events on this handle.
    pub fn set_network_trace(&mut self, enabled: bool) {
        self.network_trace = enabled;
    }

    pub(crate) fn warn(&self, warning: diagnostics::Warning) {
        self.diagnostics.warn(warning);
    }

    /// Return the merged configuration cascade for this repository.
    ///
    /// Loads through [`RepoCaches`] on every call so global, system, include, and local
    /// config stamps stay validated (not only `.git/config` mtime). Prefer
    /// [`Self::reload_config`] after in-process config writes so related caches update
    /// immediately.
    ///
    /// # Errors
    ///
    /// Propagates errors from reading or parsing config files.
    pub fn config(&self) -> Result<Arc<ConfigSet>> {
        self.ensure_config_arc()
    }

    /// Drop the in-memory config snapshot and load a fresh cascade from disk.
    ///
    /// Call after writing `.git/config` (or other cascade files) through library APIs on this
    /// repository handle so subsequent [`Self::config`] and hot-path readers observe the new data.
    ///
    /// # Errors
    ///
    /// Propagates errors from [`ConfigSet::load`].
    pub fn reload_config(&self) -> Result<()> {
        self.caches.invalidate_config_cache();
        let _ = self.config()?;
        Ok(())
    }

    /// Warm repository caches after discovery (reftable backend flag, optional config arc).
    pub(crate) fn install_config_snapshot(&self, _config: Arc<ConfigSet>) {
        let _ = self
            .caches
            .open_ref_store(&self.git_dir, self.environment.as_ref());
    }

    /// Reference storage backend for this repository.
    #[must_use]
    pub fn refs(&self) -> &dyn crate::refs::store::RefStore {
        self.ref_store.as_ref()
    }

    /// Resolve `refname` through this repository's ref store.
    ///
    /// # Errors
    ///
    /// Propagates [`crate::refs::store::RefStoreError`] as [`Error::RefStore`].
    pub fn resolve_ref_name(&self, refname: &str) -> Result<crate::objects::ObjectId> {
        self.ref_store.resolve(refname)
    }

    /// Replace the ref store (embedders injecting [`crate::refs::store::MemoryRefStore`]).
    #[must_use]
    pub fn with_ref_store(mut self, store: Arc<dyn crate::refs::store::RefStore>) -> Self {
        let key = self
            .git_dir
            .canonicalize()
            .unwrap_or_else(|_| self.git_dir.clone());
        if let Ok(mut guard) = self.caches.ref_stores.lock() {
            guard.insert(key, Arc::clone(&store));
        }
        self.ref_store = store;
        self
    }

    /// Whether pathspec matching should NFC-normalize paths for this repository.
    #[must_use]
    pub fn pathspec_precompose_enabled(&self) -> bool {
        self.caches
            .pathspec_precompose_enabled(self.environment.as_ref(), &self.git_dir)
    }

    fn cached_settings(&self) -> RepoCachedSettings {
        self.ensure_config_arc()
            .map(|cfg| {
                Self::repo_cached_settings_from_config(cfg.as_ref(), self.environment.as_ref())
            })
            .unwrap_or_else(|_| {
                Self::repo_cached_settings_from_config(&ConfigSet::new(), self.environment.as_ref())
            })
    }

    /// Open a repository from an explicit git-dir and optional work-tree.
    ///
    /// Uses [`RepositoryOptions::empty`] (no discovery overrides beyond `cwd = "."`).
    ///
    /// # Errors
    ///
    /// Returns [`Error::NotARepository`] if `git_dir` does not look like a
    /// valid git directory (missing `objects/`, `HEAD`, etc.).
    pub fn open(git_dir: &Path, work_tree: Option<&Path>) -> Result<Self> {
        Self::open_with(&RepositoryOptions::empty(), git_dir, work_tree)
    }

    /// Open a repository with an explicit [`Environment`].
    pub fn open_with(
        options: &RepositoryOptions,
        git_dir: &Path,
        work_tree: Option<&Path>,
    ) -> Result<Self> {
        let git_dir = git_dir
            .canonicalize()
            .map_err(|_| Error::NotARepository(git_dir.display().to_string()))?;

        validate_repository_format(&git_dir)?;
        let repo = Self::from_canonical_git_dir(options, git_dir, work_tree)?;
        let cfg = repo.ensure_config_arc()?;
        repo.install_config_snapshot(cfg);
        warn_core_bare_worktree_conflict(options, &repo.git_dir);
        Ok(repo)
    }

    /// Open exactly the repository at `path` for wire-protocol serving.
    ///
    /// Unlike [`Self::discover`], this does not search parent directories: a
    /// server must only serve the path it was given. The path may name a bare
    /// repository, a working tree (with `.git` beside it), or the same logical
    /// repository with a `.git` suffix appended to the path.
    ///
    /// # Parameters
    ///
    /// - `path`: client-supplied repository path (bare root, work tree, or `*.git`).
    /// - `options`: environment and injectable runners for config and hooks.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NotARepository`] when none of the candidate layouts
    /// contain a `HEAD` file.
    pub fn open_for_serving(path: &Path, options: &RepositoryOptions) -> Result<Self> {
        let mut with_git_suffix = path.as_os_str().to_owned();
        with_git_suffix.push(".git");
        let candidates = [
            (path.join(".git"), Some(path.to_path_buf())),
            (path.to_path_buf(), None),
            (PathBuf::from(with_git_suffix), None),
        ];
        for (git_dir, work_tree) in candidates {
            if !git_dir.join("HEAD").is_file() {
                continue;
            }
            return Self::open_with(options, &git_dir, work_tree.as_deref());
        }
        Err(Error::NotARepository(path.display().to_string()))
    }

    /// Like [`Self::open`] but skips repository format validation (`validate_repository_format`).
    ///
    /// Used after repository discovery when the format is unsupported so callers still learn
    /// the git directory (Git `GIT_DIR_INVALID_FORMAT` still records gitdir for `read_early_config`).
    pub fn open_skipping_format_validation(
        git_dir: &Path,
        work_tree: Option<&Path>,
    ) -> Result<Self> {
        Self::open_skipping_format_validation_with_options(
            &RepositoryOptions::empty(),
            git_dir,
            work_tree,
        )
    }

    /// Like [`Self::open_skipping_format_validation`] with full [`RepositoryOptions`].
    pub fn open_skipping_format_validation_with_options(
        options: &RepositoryOptions,
        git_dir: &Path,
        work_tree: Option<&Path>,
    ) -> Result<Self> {
        let git_dir = git_dir
            .canonicalize()
            .map_err(|_| Error::NotARepository(git_dir.display().to_string()))?;
        let repo = Self::from_canonical_git_dir(options, git_dir, work_tree)?;
        warn_core_bare_worktree_conflict(options, &repo.git_dir);
        Ok(repo)
    }

    /// Open a repository with explicit options (alias for [`Self::open_with`]).
    pub fn open_with_options(
        git_dir: &Path,
        work_tree: Option<&Path>,
        options: RepositoryOptions,
    ) -> Result<Self> {
        Self::open_with(&options, git_dir, work_tree)
    }

    /// Open a repository using a pre-built [`crate::odb::Odb`] from [`crate::odb::OdbBuilder`].
    ///
    /// The builder's [`crate::odb::OdbBuilder::files`] path must match this repository's
    /// `objects/` directory (including linked worktrees via `commondir`).
    ///
    /// # Errors
    ///
    /// Same as [`Self::open`], when the git directory is invalid.
    pub fn open_with_odb(
        options: &RepositoryOptions,
        git_dir: &Path,
        work_tree: Option<&Path>,
        builder: crate::odb::OdbBuilder,
    ) -> Result<Self> {
        let git_dir = git_dir
            .canonicalize()
            .map_err(|_| Error::NotARepository(git_dir.display().to_string()))?;

        validate_repository_format(&git_dir)?;
        let environment = Arc::new(options.environment.clone());
        let command_runner = Arc::clone(&options.command_runner);

        let head_path = git_dir.join("HEAD");
        if !head_path.exists() && !head_path.is_symlink() {
            return Err(Error::NotARepository(git_dir.display().to_string()));
        }

        let objects_dir = if git_dir.join("objects").exists() {
            git_dir.join("objects")
        } else if let Some(common_dir) = resolve_common_dir(&git_dir) {
            common_dir.join("objects")
        } else {
            return Err(Error::NotARepository(git_dir.display().to_string()));
        };

        if !objects_dir.exists() {
            return Err(Error::NotARepository(git_dir.display().to_string()));
        }

        if builder.objects_dir() != objects_dir.as_path() {
            return Err(Error::PathError(format!(
                "OdbBuilder objects dir '{}' does not match repository objects '{}'",
                builder.objects_dir().display(),
                objects_dir.display()
            )));
        }

        let work_tree = match work_tree {
            Some(p) => {
                let cwd = environment.discovery_cwd();
                let mut resolved = if p.is_absolute() {
                    p.to_path_buf()
                } else {
                    cwd.join(p)
                };
                if resolved.exists() {
                    resolved = resolved
                        .canonicalize()
                        .map_err(|_| Error::PathError(p.display().to_string()))?;
                }
                Some(resolved)
            }
            None => None,
        };

        let caches = RepoCaches::with_diagnostics(
            Arc::clone(&command_runner),
            Arc::clone(&options.diagnostics),
        );
        let odb = builder
            .build()
            .with_config_git_dir(git_dir.clone())
            .with_shared_config_state(caches.clone(), environment.clone())
            .with_resolved_work_tree(work_tree.clone());

        let git_prefix = work_tree
            .as_ref()
            .and_then(|wt| compute_git_prefix(environment.as_ref(), wt));

        let ref_store = caches.open_ref_store(&git_dir, environment.as_ref())?;

        let repo = Self {
            git_dir,
            work_tree,
            odb,
            explicit_git_dir: false,
            discovery_root: None,
            work_tree_from_env: false,
            discovery_via_gitfile: false,
            caches,
            ref_store,
            environment,
            git_prefix,
            command_runner,
            diagnostics: Arc::clone(&options.diagnostics),
            network_trace: options.network_trace,
            reference_unix_time: options.reference_unix_time,
            test_assume_different_owner: options.test_assume_different_owner,
            force_split_index: options.force_split_index,
        };
        let cfg = repo.ensure_config_arc()?;
        repo.install_config_snapshot(cfg);
        warn_core_bare_worktree_conflict(options, &repo.git_dir);
        Ok(repo)
    }

    /// Discover a repository with explicit options (alias for [`Self::discover_with`]).
    pub fn discover_with_options(start: Option<&Path>, options: RepositoryOptions) -> Result<Self> {
        Self::discover_with(&options, start)
    }

    /// Discover the repository starting from `start` (defaults to [`Environment::cwd`] if `None`).
    ///
    /// Uses [`RepositoryOptions::empty`].
    pub fn discover(start: Option<&Path>) -> Result<Self> {
        Self::discover_with(&RepositoryOptions::empty(), start)
    }

    /// Discover a repository using an explicit [`Environment`].
    pub fn discover_with(options: &RepositoryOptions, start: Option<&Path>) -> Result<Self> {
        let env = &options.environment;
        let cwd = env.discovery_cwd();

        // GIT_DIR override
        if let Some(dir) = env.git_dir.as_deref() {
            let mut git_dir = PathBuf::from(dir);
            if git_dir.is_relative() {
                git_dir = cwd.join(git_dir);
            }
            // `GIT_DIR` may name a gitfile (`.git` as a file); resolve like Git's `read_gitfile`.
            git_dir = resolve_git_dir_env_path(&git_dir)?;
            let work_tree = env.git_work_tree.as_deref().map(|wt| {
                let p = PathBuf::from(wt);
                if p.is_absolute() {
                    p
                } else {
                    cwd.join(p)
                }
            });
            if let Some(ref wt_path) = work_tree {
                if env
                    .git_work_tree
                    .as_deref()
                    .is_some_and(|raw| Path::new(raw).is_absolute())
                {
                    validate_git_work_tree_path(wt_path)?;
                }
            }
            if work_tree.is_some() {
                let mut repo = Self::open_with(options, &git_dir, work_tree.as_deref())?;
                repo.explicit_git_dir = true;
                repo.discovery_root = None;
                repo.work_tree_from_env = false;
                repo.discovery_via_gitfile = false;
                return Ok(repo);
            }
            // `GIT_DIR` without `GIT_WORK_TREE`: honour `core.bare` / `core.worktree` like Git.
            let (is_bare, core_wt) = read_core_bare_and_worktree(&git_dir)?;
            let bare_worktree_conflict = is_bare && core_wt.is_some();
            let resolved_wt = if is_bare {
                None
            } else if let Some(raw) = core_wt {
                Some(resolve_core_worktree_path(&git_dir, &raw)?)
            } else {
                // Without `GIT_WORK_TREE`, Git uses the current working directory as the work
                // tree root (see git-config(1) / `git help repository-layout`), not the parent
                // of `$GIT_DIR`. This matches upstream tests that run
                // `GIT_DIR=other/.git git …` from the top-level repo while manipulating paths
                // under `$PWD` (e.g. t5402-post-merge-hook).
                Some(cwd.canonicalize().unwrap_or_else(|_| cwd.clone()))
            };
            let mut repo = Self::open_with(options, &git_dir, resolved_wt.as_deref())?;
            repo.explicit_git_dir = true;
            repo.discovery_root = None;
            repo.work_tree_from_env = false;
            repo.discovery_via_gitfile = false;
            let _ = bare_worktree_conflict;
            return Ok(repo);
        }

        // If GIT_WORK_TREE is set without GIT_DIR, we still need to honor it
        // after discovery (path is relative to cwd, like Git).
        let env_work_tree = env.git_work_tree.as_deref().map(|wt| {
            let p = PathBuf::from(wt);
            if p.is_absolute() {
                p
            } else {
                cwd.join(p)
            }
        });
        if let Some(ref p) = env_work_tree {
            if env
                .git_work_tree
                .as_deref()
                .is_some_and(|raw| Path::new(raw).is_absolute())
            {
                validate_git_work_tree_path(p)?;
            }
        }
        let start = start.unwrap_or(&cwd);
        let start = if start.is_absolute() {
            start.to_path_buf()
        } else {
            cwd.join(start)
        };

        // Parse GIT_CEILING_DIRECTORIES — mirror Git `setup_git_directory_gently_1` +
        // `longest_ancestor_length` on the canonical cwd path.
        // A leading colon disables symlink resolution for both ceiling paths and cwd.
        let (ceiling_paths, no_resolve_ceilings) = parse_ceiling_directories(env);
        let ceiling_dirs: Vec<String> = ceiling_paths
            .into_iter()
            .map(|p| path_for_ceiling_compare(&p))
            .collect();

        let start_canon = start.canonicalize().unwrap_or_else(|_| start.clone());
        // For ceiling comparison, use non-canonical path when leading colon disables resolution.
        let ceil_cmp_buf = if no_resolve_ceilings {
            path_for_ceiling_compare(&start)
        } else {
            path_for_ceiling_compare(&start_canon)
        };
        let mut dir_buf = path_for_ceiling_compare(&start_canon);
        let min_offset = offset_1st_component(&dir_buf);
        let mut ceil_offset: isize = longest_ancestor_length(&ceil_cmp_buf, &ceiling_dirs)
            .map(|n| n as isize)
            .unwrap_or(-1);
        if ceiling_dirs.contains(&dir_buf) {
            ceil_offset = dir_buf.len() as isize;
        } else if ceil_offset < 0 {
            ceil_offset = min_offset as isize - 2;
        }

        loop {
            let current = Path::new(&dir_buf);
            if let Some(DiscoveredAt { mut repo, gitfile }) = try_open_at(options, current)? {
                // git/setup.c `setup_git_directory` runs `check_repository_format` on the resolved
                // git dir and dies on a bad format (e.g. a v1-only `extensions.*` in a
                // `repositoryformatversion = 0` repo; t0001 #60). Discovery itself opens with
                // validation skipped so an empty `.git/` is walked past, but a *found* repository
                // must satisfy the format check.
                validate_repository_format(&repo.git_dir)?;
                repo.environment = Arc::new(env.clone());
                let load_opts = LoadConfigOptions {
                    include_system: true,
                    include_ctx: crate::config::IncludeContext {
                        git_dir: Some(repo.git_dir.clone()),
                        cwd: env.cwd.clone(),
                        pwd: env.pwd.clone(),
                        ..Default::default()
                    },
                    diagnostics: Some(Arc::clone(&options.diagnostics)),
                    ..Default::default()
                };
                let cfg = Arc::new(ConfigSet::load_with_options(
                    env,
                    Some(&repo.git_dir),
                    &load_opts,
                )?);
                repo.install_config_snapshot(Arc::clone(&cfg));
                repo.set_diagnostics(Arc::clone(&options.diagnostics));
                if let Some(ref wt) = env_work_tree {
                    repo.work_tree = Some(wt.canonicalize().unwrap_or_else(|_| wt.clone()));
                    repo.work_tree_from_env = true;
                } else {
                    repo.work_tree_from_env = false;
                    // Linked worktree (gitfile → admin dir with `commondir`): `Repository::open`
                    // already set `work_tree` to the directory that contains the `.git` file.
                    // Do not replace it with `core.worktree` from the common config — it may be
                    // stale (t1501 multi-worktree) or point at another linked checkout.
                    let linked_gitfile =
                        repo.discovery_via_gitfile && resolve_common_dir(&repo.git_dir).is_some();
                    if !linked_gitfile {
                        let (is_bare, core_wt) = read_core_bare_and_worktree_from_config(&cfg);
                        if is_bare {
                            repo.work_tree = None;
                        } else if let Some(raw) = core_wt {
                            repo.work_tree = Some(resolve_core_worktree_path(&repo.git_dir, &raw)?);
                        }
                    }
                }
                let assume_different =
                    options.test_assume_different_owner || env.test_assume_different_owner();
                if assume_different {
                    repo.enforce_safe_directory()?;
                } else {
                    ensure_valid_ownership(
                        env,
                        gitfile.as_deref(),
                        repo.work_tree.as_deref(),
                        &repo.git_dir,
                    )?;
                }
                if let Some(wt) = repo.work_tree.as_ref() {
                    repo.git_prefix = compute_git_prefix(env, wt);
                }
                return Ok(repo);
            }

            let mut offset: isize = dir_buf.len() as isize;
            if offset <= min_offset as isize {
                break;
            }
            loop {
                offset -= 1;
                if offset <= ceil_offset {
                    break;
                }
                if dir_buf
                    .as_bytes()
                    .get(offset as usize)
                    .is_some_and(|b| *b == b'/')
                {
                    break;
                }
            }
            if offset <= ceil_offset {
                break;
            }
            let off_u = offset as usize;
            let new_len = if off_u > min_offset {
                off_u
            } else {
                min_offset
            };
            dir_buf.truncate(new_len);
        }

        Err(Error::NotARepository(start.display().to_string()))
    }

    /// Current directory to use for pathspec / cwd-prefix logic.
    ///
    /// When `GIT_WORK_TREE` points at a directory that does not contain the process cwd
    /// (alternate work tree + index from the main repo directory), Git treats pathspecs as
    /// relative to the work tree root — use that root as the effective cwd.
    #[must_use]
    pub fn effective_pathspec_cwd(&self) -> PathBuf {
        let cwd = self.environment.discovery_cwd();
        let Some(wt) = self.work_tree.as_ref() else {
            return cwd;
        };
        let inside_lexical = cwd.strip_prefix(wt).is_ok();
        let inside_canon = cwd
            .canonicalize()
            .ok()
            .zip(wt.canonicalize().ok())
            .is_some_and(|(c, w)| c.starts_with(&w));
        if inside_lexical || inside_canon {
            cwd
        } else {
            wt.clone()
        }
    }

    /// Path to the index file.
    #[must_use]
    pub fn index_path(&self) -> PathBuf {
        self.git_dir.join("index")
    }

    /// Resolve which index file to use, honouring `GIT_INDEX_FILE` like Git plumbing.
    ///
    /// Relative paths are resolved from the process current directory.
    pub fn index_path_for_env(&self) -> Result<PathBuf> {
        if let Some(raw) = self.environment.git_index_file.as_deref() {
            if !raw.is_empty() {
                let p = PathBuf::from(raw);
                return Ok(if p.is_absolute() {
                    p
                } else {
                    self.environment.discovery_cwd().join(p)
                });
            }
        }
        Ok(self.index_path())
    }

    /// Load the index, expanding sparse-directory placeholders from the object database.
    ///
    /// Commands that operate on individual paths should use this instead of [`Index::load`].
    pub fn load_index(&self) -> Result<Index> {
        let path = self.index_path_for_env()?;
        self.load_index_at(&path)
    }

    /// Like [`Repository::load_index`], but reads from an explicit index file path
    /// (e.g. `GIT_INDEX_FILE` or a worktree-specific index).
    pub fn load_index_at(&self, path: &std::path::Path) -> Result<Index> {
        let cfg = self.config().unwrap_or_else(|_| Arc::new(ConfigSet::new()));
        if let Some(res) = cfg.as_ref().get_bool("index.sparse") {
            res.map_err(|s| Error::Config(s.into()))?;
        }
        let mut idx = Index::load_expand_sparse_optional(path, &self.odb)?;
        crate::split_index::resolve_split_index_if_needed(&mut idx, &self.git_dir, path)?;
        if idx.source_mtime.is_none() {
            idx.source_mtime = crate::index::index_file_mtime(path);
        }
        if let Some(ref wt) = self.work_tree {
            crate::sparse_checkout::clear_skip_worktree_from_present_files(
                &self.git_dir,
                wt,
                &mut idx,
                Some(cfg.as_ref()),
            );
        }
        Ok(idx)
    }

    /// Write the index to the default path after optionally collapsing skip-worktree
    /// subtrees into sparse-directory placeholders (when sparse index is enabled).
    pub fn write_index(&self, index: &mut Index) -> Result<()> {
        self.write_index_at(&self.index_path(), index)
    }

    /// Write a pack reachability `.bitmap` sidecar for `pack_idx_path`.
    ///
    /// # Errors
    ///
    /// Returns [`crate::pack_bitmap::PackBitmapWriteError`] when the pack is not
    /// closed under reachability from the repository refs used for validation.
    pub fn write_pack_bitmap(
        &self,
        pack_idx_path: &std::path::Path,
        options: &crate::pack_bitmap::PackBitmapWriteOptions,
        now: std::time::SystemTime,
    ) -> std::result::Result<std::path::PathBuf, crate::pack_bitmap::PackBitmapWriteError> {
        crate::pack_bitmap::PackBitmapWriter::write(self, pack_idx_path, options, now)
    }

    /// Acquire `.git/index.lock` before mutating the working tree for index+worktree commands.
    ///
    /// Call [`Self::commit_index_update`] to persist the index and release the lock, or drop the
    /// lock without committing to abort without writing.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] when the lock file already exists or cannot be created.
    pub fn begin_index_update(&self) -> Result<crate::index::IndexLock> {
        test_inject_index_write_fail()?;
        let path = self.index_path();
        let cfg = self.config()?;
        crate::index::IndexLock::acquire(&path, Some(cfg.as_ref()))
    }

    /// Write `index` to disk using a lock from [`Self::begin_index_update`].
    ///
    /// # Errors
    ///
    /// Propagates index finalization, serialization, and I/O failures.
    pub fn commit_index_update(
        &self,
        lock: &mut crate::index::IndexLock,
        index: &mut Index,
        updated_workdir: bool,
    ) -> Result<()> {
        self.flush_index_update(lock, index)?;
        self.finish_index_update(lock, updated_workdir)
    }

    /// Serialize `index` into a held [`crate::index::IndexLock`] without replacing the on-disk index.
    pub fn flush_index_update(
        &self,
        lock: &mut crate::index::IndexLock,
        index: &mut Index,
    ) -> Result<()> {
        test_inject_index_write_fail()?;
        index.hash_algo = self.odb.hash_algo();
        self.finalize_sparse_index_if_needed(index)?;
        let path = self.index_path();
        let prev_index_mtime = crate::index::index_file_mtime(&path);
        let cfg = self.config()?;
        if let Some(work_tree) = self.work_tree.as_deref() {
            crate::diff::smudge_racily_clean_entries(
                &self.odb,
                &self.git_dir,
                index,
                work_tree,
                prev_index_mtime,
                Some(cfg.as_ref()),
                Some(self.caches().filters()),
            );
        }
        let skip_hash = crate::index::index_skip_hash_for_write(Some(cfg.as_ref()));
        index.write_to_path_with_config_and_lock_mode(
            &path,
            skip_hash,
            Some(cfg.as_ref()),
            Some(lock),
            false,
        )
    }

    /// Commit a staged index lock after a successful working-tree update.
    pub fn finish_index_update(
        &self,
        lock: &mut crate::index::IndexLock,
        updated_workdir: bool,
    ) -> Result<()> {
        lock.commit_replace_index()?;
        let updated_workdir_arg = if updated_workdir { "1" } else { "0" };
        let _ = run_hook(self, "post-index-change", &[updated_workdir_arg, "0"], None);
        Ok(())
    }

    /// Persist the index when the lock can be acquired (Git `repo_update_index_if_able`).
    ///
    /// Opportunistic writers such as `status` stat refresh call this instead of
    /// [`Repository::write_index`]: a held `.git/index.lock` or an unwritable repository
    /// directory must not fail the parent operation.
    ///
    /// Returns `Ok(true)` when the index was written, `Ok(false)` when persistence was skipped.
    ///
    /// # Errors
    ///
    /// Propagates non-recoverable failures (for example cache-tree verification or split-index
    /// constraints).
    pub fn try_write_index(&self, index: &mut Index) -> Result<bool> {
        match self.write_index(index) {
            Ok(()) => Ok(true),
            Err(e) if Self::opportunistic_index_write_skippable(&e) => Ok(false),
            Err(e) => Err(e),
        }
    }

    fn opportunistic_index_write_skippable(err: &crate::error::Error) -> bool {
        use crate::error::Error;
        match err {
            Error::Io(e) => matches!(
                e.kind(),
                std::io::ErrorKind::AlreadyExists
                    | std::io::ErrorKind::PermissionDenied
                    | std::io::ErrorKind::ReadOnlyFilesystem
            ),
            _ => false,
        }
    }

    /// Write the index to the default path and pass explicit `post-index-change` hook flags.
    ///
    /// Parameters:
    /// - `index` is the in-memory index to serialize.
    /// - `updated_workdir` reports that the write is paired with a working-tree update.
    /// - `updated_skipworktree` reports that skip-worktree related index state changed.
    ///
    /// Returns `Ok(())` after the index is written and the hook has been attempted.
    ///
    /// Errors when the index cannot be finalized or written.
    pub fn write_index_with_post_index_change(
        &self,
        index: &mut Index,
        updated_workdir: bool,
        updated_skipworktree: bool,
    ) -> Result<()> {
        self.write_index_at_with_post_index_change(
            &self.index_path(),
            index,
            updated_workdir,
            updated_skipworktree,
        )
    }

    /// Like [`Repository::write_index`], but writes to an explicit index file path.
    pub fn write_index_at(&self, path: &std::path::Path, index: &mut Index) -> Result<()> {
        self.write_index_at_split(path, index, WriteSplitIndexRequest::default())
    }

    /// Whether reading `index` under this repository's config would mark the cache changed solely to
    /// materialize a split index.
    ///
    /// Git's `tweak_split_index` runs after every index read: when `core.splitIndex` is `true` and the
    /// index is not yet a split index, `add_split_index` sets `SPLIT_INDEX_ORDERED` on `cache_changed`,
    /// which makes opportunistic writers such as `status` (`repo_update_index_if_able`) rewrite the
    /// index in split form, creating `.git/sharedindex.<oid>`. This returns `true` exactly when that
    /// would happen: split index is requested (config `true` or `GIT_TEST_SPLIT_INDEX`) and the
    /// supplied `index` does not already carry a `link` extension.
    ///
    /// Returns `false` when split index is disabled/unset or the index is already split.
    #[must_use]
    pub fn split_index_would_force_write(&self, index: &Index) -> bool {
        if index.split_index_base_oid().is_some() {
            return false;
        }
        let cfg = self.config().unwrap_or_else(|_| Arc::new(ConfigSet::new()));
        matches!(
            crate::split_index::split_index_config(cfg.as_ref()),
            crate::split_index::SplitIndexConfig::Enabled
        ) || self.force_split_index
            || crate::split_index::git_test_split_index_env()
    }

    /// Like [`Repository::write_index_at`], but passes explicit `post-index-change` hook flags.
    ///
    /// Parameters:
    /// - `path` is the destination index file.
    /// - `index` is the in-memory index to serialize.
    /// - `updated_workdir` reports that the write is paired with a working-tree update.
    /// - `updated_skipworktree` reports that skip-worktree related index state changed.
    ///
    /// Returns `Ok(())` after the index is written and the hook has been attempted.
    ///
    /// Errors when the index cannot be finalized or written.
    pub fn write_index_at_with_post_index_change(
        &self,
        path: &std::path::Path,
        index: &mut Index,
        updated_workdir: bool,
        updated_skipworktree: bool,
    ) -> Result<()> {
        self.write_index_at_split_with_post_index_change(
            path,
            index,
            WriteSplitIndexRequest::default(),
            updated_workdir,
            updated_skipworktree,
        )
    }

    /// Write the index to `path`, optionally emitting a split index (shared base + `link` extension).
    pub fn write_index_at_split(
        &self,
        path: &std::path::Path,
        index: &mut Index,
        split: WriteSplitIndexRequest,
    ) -> Result<()> {
        self.write_index_at_split_with_post_index_change(path, index, split, false, false)
    }

    /// Write the index to `path`, optionally emitting a split index, with explicit hook flags.
    ///
    /// Parameters:
    /// - `path` is the destination index file.
    /// - `index` is the in-memory index to serialize.
    /// - `split` controls whether a split index should be written.
    /// - `updated_workdir` reports that the write is paired with a working-tree update.
    /// - `updated_skipworktree` reports that skip-worktree related index state changed.
    ///
    /// Returns `Ok(())` after the index is written and the hook has been attempted.
    ///
    /// Errors when the index cannot be finalized or written.
    pub fn write_index_at_split_with_post_index_change(
        &self,
        path: &std::path::Path,
        index: &mut Index,
        split: WriteSplitIndexRequest,
        updated_workdir: bool,
        updated_skipworktree: bool,
    ) -> Result<()> {
        test_inject_index_write_fail()?;
        // The on-disk index format (entry OID width and trailing checksum) is
        // fixed by the repository's hash algorithm. Stamp it here so every
        // index written through the repository is consistent, regardless of how
        // the in-memory `Index` was constructed (e.g. a fresh `Index::new`).
        index.hash_algo = self.odb.hash_algo();
        self.finalize_sparse_index_if_needed(index)?;
        let prev_index_mtime = crate::index::index_file_mtime(path);
        let cfg = self.config()?;
        if let Some(work_tree) = self.work_tree.as_deref() {
            crate::diff::smudge_racily_clean_entries(
                &self.odb,
                &self.git_dir,
                index,
                work_tree,
                prev_index_mtime,
                Some(cfg.as_ref()),
                Some(self.caches().filters()),
            );
        }
        let skip_hash = crate::index::index_skip_hash_for_write(Some(cfg.as_ref()));
        write_index_file_split(path, &self.git_dir, index, cfg.as_ref(), split, skip_hash)?;
        // Git `write_locked_index`: `post-index-change` after a successful index write (t1800).
        let updated_workdir_arg = if updated_workdir { "1" } else { "0" };
        let updated_skipworktree_arg = if updated_skipworktree { "1" } else { "0" };
        let _ = run_hook(
            self,
            "post-index-change",
            &[updated_workdir_arg, updated_skipworktree_arg],
            None,
        );
        Ok(())
    }

    fn finalize_sparse_index_if_needed(&self, index: &mut Index) -> Result<()> {
        let cfg = self.config().unwrap_or_else(|_| Arc::new(ConfigSet::new()));
        let sparse_enabled = cfg
            .get("core.sparseCheckout")
            .map(|v| v == "true")
            .unwrap_or(false);
        if !sparse_enabled {
            index.sparse_directories = false;
            return Ok(());
        }
        let cone_cfg = cfg
            .get("core.sparseCheckoutCone")
            .and_then(|v| v.parse::<bool>().ok())
            .unwrap_or(true);
        let sparse_ix = cfg
            .get("index.sparse")
            .map(|v| v == "true")
            .unwrap_or(false);
        let patterns = read_sparse_checkout_patterns(&self.git_dir);
        let cone = effective_cone_mode_for_sparse_file(cone_cfg, &patterns);
        let head = resolve_head(&self.git_dir)?;
        let tree_oid = if let Some(oid) = head.oid() {
            let obj = self.odb.read(oid)?;
            let commit = parse_commit(&obj.data)?;
            Some(commit.tree)
        } else {
            None
        };
        if let Some(t) = tree_oid {
            index.try_collapse_sparse_directories(&self.odb, &t, &patterns, cone, sparse_ix)?;
        } else {
            index.sparse_directories = false;
        }
        Ok(())
    }

    /// Path to the `refs/` directory.
    #[must_use]
    pub fn refs_dir(&self) -> PathBuf {
        self.git_dir.join("refs")
    }

    /// Path to `HEAD`.
    #[must_use]
    pub fn head_path(&self) -> PathBuf {
        self.git_dir.join("HEAD")
    }

    /// Relative path from the work tree root to the process current directory, `/`-separated.
    ///
    /// Used for `:(top)` / `:/` pathspec Bloom lookups. Returns `None` for bare repositories or
    /// when paths cannot be resolved; callers should treat `None` like an empty prefix.
    #[must_use]
    pub fn bloom_pathspec_cwd(&self) -> Option<String> {
        let wt = self.work_tree.as_ref()?;
        let cwd = self.environment.discovery_cwd();
        let wt = wt.canonicalize().ok()?;
        let cwd = cwd.canonicalize().ok()?;
        let rel = cwd.strip_prefix(&wt).ok()?;
        let s = rel.to_string_lossy().replace('\\', "/");
        let s = s.trim_start_matches('/').to_string();
        Some(s)
    }

    /// Whether this is a bare repository (no working tree).
    #[must_use]
    pub fn is_bare(&self) -> bool {
        if let Ok(cfg) = self.config() {
            if let Some(Ok(bare)) = cfg.get_bool("core.bare") {
                return bare;
            }
        }
        self.work_tree.is_none()
    }

    /// Read an object, transparently following replace refs.
    ///
    /// If `refs/replace/<hex>` exists for the requested OID and
    /// `GIT_NO_REPLACE_OBJECTS` is **not** set, this reads the
    /// replacement object instead.  Otherwise it behaves identically
    /// to `self.odb.read(oid)`.
    pub fn read_replaced(&self, oid: &crate::objects::ObjectId) -> Result<crate::objects::Object> {
        if self.environment.git_no_replace_objects {
            return self.odb.read(oid);
        }
        let settings = self.cached_settings();
        if !settings.use_replace_refs {
            return self.odb.read(oid);
        }
        let replace_ref =
            self.git_dir
                .join(format!("{}{}", settings.replace_ref_base, oid.to_hex()));
        if replace_ref.is_file() {
            if let Ok(content) = std::fs::read_to_string(&replace_ref) {
                let hex = content.trim();
                if let Ok(replacement_oid) = hex.parse::<crate::objects::ObjectId>() {
                    if let Ok(obj) = self.odb.read(&replacement_oid) {
                        return Ok(obj);
                    }
                }
            }
        }
        self.odb.read(oid)
    }
}

/// If `GIT_TRACE_SETUP` is an absolute path, append `setup:` lines (Git test format).
///
/// Upstream tests grep `^setup: ` from the trace file; they do not use the timestamped
/// `trace.c:` prefix that full Git tracing adds.
pub fn trace_repo_setup_if_requested(repo: &Repository) -> std::io::Result<()> {
    let Some(path) = repo.environment.git_trace_setup.as_deref() else {
        return Ok(());
    };
    if path.is_empty() || path == "0" {
        return Ok(());
    }
    let trace_path = Path::new(path);
    if !trace_path.is_absolute() {
        return Ok(());
    }

    let actual_cwd = repo.environment.discovery_cwd();
    let actual_cwd = actual_cwd
        .canonicalize()
        .unwrap_or_else(|_| actual_cwd.clone());

    // After setup, Git's traced `cwd` is the worktree root when the process cwd started inside
    // the worktree, but stays at the real cwd when outside (t1510 nephew cases).
    let (trace_cwd, prefix) = if let Some(ref wt) = repo.work_tree {
        let wt_canon = wt.canonicalize().unwrap_or_else(|_| wt.clone());
        if actual_cwd.starts_with(&wt_canon) {
            let rel = actual_cwd
                .strip_prefix(&wt_canon)
                .map(|p| p.to_path_buf())
                .unwrap_or_default();
            let prefix = if rel.as_os_str().is_empty() {
                "(null)".to_owned()
            } else {
                let mut s = rel.to_string_lossy().replace('\\', "/");
                if !s.ends_with('/') {
                    s.push('/');
                }
                s
            };
            (wt_canon, prefix)
        } else {
            (actual_cwd.clone(), "(null)".to_owned())
        }
    } else {
        (actual_cwd.clone(), "(null)".to_owned())
    };

    let git_dir_display =
        display_git_dir_for_setup_trace(repo, &trace_cwd, &actual_cwd, prefix.as_str());
    let common_display = display_common_dir_for_setup_trace(
        repo,
        &trace_cwd,
        &actual_cwd,
        prefix.as_str(),
        &git_dir_display,
    );
    let worktree_display = repo
        .work_tree
        .as_ref()
        .map(|p| {
            p.canonicalize()
                .unwrap_or_else(|_| lexical_normalize_path(p))
                .display()
                .to_string()
        })
        .unwrap_or_else(|| "(null)".to_owned());

    let mut f = OpenOptions::new()
        .create(true)
        .append(true)
        .open(trace_path)?;
    writeln!(f, "setup: git_dir: {git_dir_display}")?;
    writeln!(f, "setup: git_common_dir: {common_display}")?;
    writeln!(f, "setup: worktree: {worktree_display}")?;
    writeln!(f, "setup: cwd: {}", trace_cwd.display())?;
    writeln!(f, "setup: prefix: {prefix}")?;
    Ok(())
}

/// Collapse `.` / `..` in a path for display when `canonicalize()` fails (e.g. non-existent `..` segments).
fn lexical_normalize_path(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    let mut absolute = false;
    for c in path.components() {
        match c {
            Component::Prefix(p) => {
                out.push(p.as_os_str());
            }
            Component::RootDir => {
                absolute = true;
                out.push(c.as_os_str());
            }
            Component::CurDir => {}
            Component::ParentDir => {
                if absolute {
                    let _ = out.pop();
                } else if !out.pop() {
                    out.push("..");
                }
            }
            Component::Normal(s) => out.push(s),
        }
    }
    if out.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        out
    }
}

/// Path from `base` to `target` using `..` segments when needed (matches Git setup traces).
fn path_relative_to(target: &Path, base: &Path) -> Option<PathBuf> {
    let t = target.canonicalize().ok()?;
    let b = base.canonicalize().ok()?;
    let tc: Vec<_> = t.components().collect();
    let bc: Vec<_> = b.components().collect();
    let mut i = 0usize;
    while i < tc.len() && i < bc.len() && tc[i] == bc[i] {
        i += 1;
    }
    let up = bc.len().saturating_sub(i);
    let mut out = PathBuf::new();
    for _ in 0..up {
        out.push("..");
    }
    for comp in &tc[i..] {
        out.push(comp.as_os_str());
    }
    Some(out)
}

fn rel_path_for_setup_trace(target: &Path, trace_cwd: &Path) -> String {
    let t = target
        .canonicalize()
        .unwrap_or_else(|_| target.to_path_buf());
    let tc = trace_cwd
        .canonicalize()
        .unwrap_or_else(|_| trace_cwd.to_path_buf());
    if let Some(rel) = path_relative_to(&t, &tc) {
        let s = rel.to_string_lossy().replace('\\', "/");
        return if s.is_empty() || s == "." {
            ".".to_owned()
        } else {
            s
        };
    }
    t.display().to_string()
}

fn trace_cwd_strictly_inside_git_parent(trace_cwd: &Path, git_dir: &Path) -> bool {
    let tc = trace_cwd
        .canonicalize()
        .unwrap_or_else(|_| trace_cwd.to_path_buf());
    let gd = git_dir
        .canonicalize()
        .unwrap_or_else(|_| git_dir.to_path_buf());
    let Some(parent) = gd.parent() else {
        return false;
    };
    let parent = parent.to_path_buf();
    if tc == parent {
        return false;
    }
    tc.starts_with(&parent) && tc != parent
}

fn display_git_dir_for_setup_trace(
    repo: &Repository,
    trace_cwd: &Path,
    actual_cwd: &Path,
    setup_prefix: &str,
) -> String {
    let gd = repo
        .git_dir
        .canonicalize()
        .unwrap_or_else(|_| repo.git_dir.clone());
    let tc = trace_cwd
        .canonicalize()
        .unwrap_or_else(|_| trace_cwd.to_path_buf());
    let ac = actual_cwd
        .canonicalize()
        .unwrap_or_else(|_| actual_cwd.to_path_buf());

    // Bare repo discovered without `GIT_DIR`: cwd inside the git directory (t1510 #16).
    // Trace uses `.` at the git-dir root and the absolute git-dir path from subdirectories.
    if repo.work_tree.is_none() && !repo.explicit_git_dir {
        if ac == gd {
            return ".".to_owned();
        }
        if ac.starts_with(&gd) && ac != gd {
            return gd.display().to_string();
        }
    }

    // Non-bare repo with `core.worktree` while cwd is inside the git-dir (t1510 #20a).
    if !repo.explicit_git_dir {
        if let Some(wt) = &repo.work_tree {
            let wt = wt.canonicalize().unwrap_or_else(|_| wt.clone());
            if ac.starts_with(&gd) && ac != wt {
                return gd.display().to_string();
            }
        }
    }

    // `GIT_DIR` set: Git's `set_git_dir(gitdirenv, make_realpath)` keeps a relative
    // `gitdirenv` only when cwd is at the worktree root or outside the worktree; from a
    // subdirectory it realpath()s to an absolute path (see `setup.c` / t1510).
    if repo.explicit_git_dir {
        if repo.work_tree.is_none() {
            if let Some(raw) = repo.environment.git_dir.as_deref() {
                let p = Path::new(raw.trim());
                if p.is_absolute() {
                    return gd.display().to_string();
                }
                let joined = ac.join(p);
                if joined.is_file() {
                    return gd.display().to_string();
                }
                if let Some(rel) = path_relative_to(&gd, &tc) {
                    let s = rel.to_string_lossy().replace('\\', "/");
                    return if s.is_empty() || s == "." {
                        ".".to_owned()
                    } else {
                        s
                    };
                }
            }
            return gd.display().to_string();
        }
        if let Some(wt) = &repo.work_tree {
            let wt = wt.canonicalize().unwrap_or_else(|_| wt.clone());
            let strictly_inside_wt = ac.starts_with(&wt) && ac != wt;
            if strictly_inside_wt {
                return gd.display().to_string();
            }
            if let Some(raw) = repo.environment.git_dir.as_deref() {
                let p = Path::new(raw.trim());
                if p.is_relative() {
                    let joined = ac.join(p);
                    if joined.is_file() {
                        // `GIT_DIR` points at a gitfile; trace shows the resolved git dir.
                        return gd.display().to_string();
                    }
                    if let Some(rel) = path_relative_to(&gd, &tc) {
                        let s = rel.to_string_lossy().replace('\\', "/");
                        return if s.is_empty() || s == "." {
                            ".".to_owned()
                        } else {
                            s
                        };
                    }
                }
                return gd.display().to_string();
            }
        }
        if trace_cwd_strictly_inside_git_parent(trace_cwd, &gd) {
            return rel_path_for_setup_trace(&gd, trace_cwd);
        }
        return gd.display().to_string();
    }

    let work_relocated = match (&repo.discovery_root, &repo.work_tree) {
        (Some(root), Some(wt)) if !repo.work_tree_from_env => {
            let r = root.canonicalize().unwrap_or_else(|_| root.clone());
            let w = wt.canonicalize().unwrap_or_else(|_| wt.clone());
            r != w
        }
        _ => false,
    };

    if repo.work_tree_from_env {
        if !repo.discovery_via_gitfile {
            if setup_prefix == "(null)" {
                if let (Some(root), Some(wt)) = (&repo.discovery_root, &repo.work_tree) {
                    let r = root.canonicalize().unwrap_or_else(|_| root.clone());
                    let w = wt.canonicalize().unwrap_or_else(|_| wt.clone());
                    if r == w {
                        let dot_git = r.join(".git");
                        let dot_git = dot_git.canonicalize().unwrap_or(dot_git);
                        if gd == dot_git {
                            return ".git".to_owned();
                        }
                    }
                }
            }
            if trace_cwd_strictly_inside_git_parent(trace_cwd, &gd) {
                return rel_path_for_setup_trace(&gd, trace_cwd);
            }
        }
        return gd.display().to_string();
    }

    if work_relocated {
        if let Some(wt) = &repo.work_tree {
            let wt = wt.canonicalize().unwrap_or_else(|_| wt.clone());
            if ac == wt {
                return gd.display().to_string();
            }
            let inside_wt = ac.starts_with(&wt) && ac != wt;
            if inside_wt {
                if let Some(rel) = path_relative_to(&gd, &ac) {
                    let s = rel.to_string_lossy().replace('\\', "/");
                    return if s.is_empty() || s == "." {
                        ".".to_owned()
                    } else {
                        s
                    };
                }
            }
        }
    }
    if repo.work_tree.is_some() {
        if let Some(root) = &repo.discovery_root {
            let r = root.canonicalize().unwrap_or_else(|_| root.clone());
            let dot_git = r.join(".git");
            let dot_git = dot_git.canonicalize().unwrap_or(dot_git);
            if gd == dot_git {
                return ".git".to_owned();
            }
        } else if let Some(wt) = &repo.work_tree {
            let wt = wt.canonicalize().unwrap_or_else(|_| wt.clone());
            let dot_git = wt.join(".git");
            let dot_git = dot_git.canonicalize().unwrap_or(dot_git);
            if gd == dot_git {
                return ".git".to_owned();
            }
        }
    }

    if repo.discovery_via_gitfile && !repo.explicit_git_dir {
        return gd.display().to_string();
    }

    // Bare repo whose git-dir is `parent/.git`: at `parent` the trace shows `.git`; from a
    // subdirectory of `parent` that is still outside `.git`, Git uses the absolute git-dir (t1510
    // #16c sub/ case — not `../.git`).
    if repo.work_tree.is_none() && !repo.explicit_git_dir {
        if let Some(gp) = gd.parent() {
            let gp = gp.canonicalize().unwrap_or_else(|_| gp.to_path_buf());
            let gdc = gd.canonicalize().unwrap_or_else(|_| gd.clone());
            if tc.starts_with(&gp) && tc != gp && !tc.starts_with(&gdc) {
                return gdc.display().to_string();
            }
            if tc == gp {
                return rel_path_for_setup_trace(&gd, trace_cwd);
            }
        }
    }

    if trace_cwd_strictly_inside_git_parent(trace_cwd, &gd) {
        rel_path_for_setup_trace(&gd, trace_cwd)
    } else {
        gd.display().to_string()
    }
}

fn display_common_dir_for_setup_trace(
    repo: &Repository,
    trace_cwd: &Path,
    actual_cwd: &Path,
    _setup_prefix: &str,
    git_dir_display: &str,
) -> String {
    let gd = repo
        .git_dir
        .canonicalize()
        .unwrap_or_else(|_| repo.git_dir.clone());
    let Some(common) = resolve_common_dir(&gd) else {
        return git_dir_display.to_owned();
    };
    let common = common.canonicalize().unwrap_or(common);
    if common == gd {
        return git_dir_display.to_owned();
    }

    let ac = actual_cwd
        .canonicalize()
        .unwrap_or_else(|_| actual_cwd.to_path_buf());
    if repo.work_tree.is_none() && !repo.explicit_git_dir {
        if ac == common {
            return ".".to_owned();
        }
        if ac.starts_with(&common) && ac != common {
            return common.display().to_string();
        }
    }

    let work_relocated = match (&repo.discovery_root, &repo.work_tree) {
        (Some(root), Some(wt)) if !repo.work_tree_from_env => {
            let r = root.canonicalize().unwrap_or_else(|_| root.clone());
            let w = wt.canonicalize().unwrap_or_else(|_| wt.clone());
            r != w
        }
        _ => false,
    };
    if work_relocated {
        if let Some(wt) = &repo.work_tree {
            let wt = wt.canonicalize().unwrap_or_else(|_| wt.clone());
            if ac == wt {
                return common.display().to_string();
            }
            let inside_wt = ac.starts_with(&wt) && ac != wt;
            if inside_wt {
                if let Some(rel) = path_relative_to(&common, &ac) {
                    let s = rel.to_string_lossy().replace('\\', "/");
                    return if s.is_empty() || s == "." {
                        ".".to_owned()
                    } else {
                        s
                    };
                }
            }
        }
    }

    if repo.discovery_via_gitfile && !repo.explicit_git_dir {
        return common.display().to_string();
    }

    if repo.work_tree.is_none() && !repo.explicit_git_dir {
        let tc = trace_cwd
            .canonicalize()
            .unwrap_or_else(|_| trace_cwd.to_path_buf());
        if let Some(cp) = common.parent() {
            let cp = cp.canonicalize().unwrap_or_else(|_| cp.to_path_buf());
            let comc = common.canonicalize().unwrap_or_else(|_| common.clone());
            if tc.starts_with(&cp) && tc != cp && !tc.starts_with(&comc) {
                return comc.display().to_string();
            }
            if tc == cp {
                return rel_path_for_setup_trace(&common, trace_cwd);
            }
        }
    }

    if trace_cwd_strictly_inside_git_parent(trace_cwd, &common) {
        rel_path_for_setup_trace(&common, trace_cwd)
    } else {
        common.display().to_string()
    }
}

/// Resolve the common git directory for linked worktrees.
fn resolve_common_dir(git_dir: &Path) -> Option<PathBuf> {
    let common_raw = fs::read_to_string(git_dir.join("commondir")).ok()?;
    let common_rel = common_raw.trim();
    if common_rel.is_empty() {
        return None;
    }
    let common_dir = if Path::new(common_rel).is_absolute() {
        PathBuf::from(common_rel)
    } else {
        git_dir.join(common_rel)
    };
    Some(common_dir.canonicalize().unwrap_or(common_dir))
}

/// Directory holding `config` for early-config reads (`commondir` when present).
#[must_use]
pub fn common_git_dir_for_config(git_dir: &Path) -> PathBuf {
    resolve_common_dir(git_dir).unwrap_or_else(|| git_dir.to_path_buf())
}

/// True when `extensions.worktreeConfig` is enabled in the common `config`.
pub fn worktree_config_enabled(common_dir: &Path) -> bool {
    let path = common_dir.join("config");
    let Ok(content) = fs::read_to_string(&path) else {
        return false;
    };
    let mut in_extensions = false;
    for raw_line in content.lines() {
        let mut line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if line.starts_with('[') {
            let Some(end_idx) = line.find(']') else {
                continue;
            };
            let section = line[1..end_idx].trim();
            let section_name = section
                .split_whitespace()
                .next()
                .unwrap_or_default()
                .to_ascii_lowercase();
            in_extensions = section_name == "extensions";
            let remainder = line[end_idx + 1..].trim();
            if remainder.is_empty() || remainder.starts_with('#') || remainder.starts_with(';') {
                continue;
            }
            line = remainder;
        }
        if in_extensions {
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            if key.trim().eq_ignore_ascii_case("worktreeconfig") {
                let v = value.trim();
                return v.eq_ignore_ascii_case("true")
                    || v.eq_ignore_ascii_case("yes")
                    || v.eq_ignore_ascii_case("on")
                    || v == "1";
            }
        }
    }
    false
}

fn open_or_create_config_file(path: &Path, scope: ConfigScope) -> Result<ConfigFile> {
    match ConfigFile::from_path(path, scope)? {
        Some(f) => Ok(f),
        None => {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).map_err(Error::Io)?;
            }
            ConfigFile::parse(path, "", scope)
        }
    }
}

fn config_file_bool_true(cfg: &ConfigFile, key: &str) -> bool {
    cfg.get(key).is_some_and(|v| {
        matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "true" | "yes" | "on" | "1"
        )
    })
}

/// Enable per-worktree configuration (`extensions.worktreeConfig`) and create
/// `config.worktree`, matching Git's `init_worktree_config` in `worktree.c`.
///
/// When `core.bare` is true or `core.worktree` is set in the common config,
/// those keys are moved into `config.worktree` so linked worktrees keep working.
///
/// # Errors
///
/// Returns [`Error::Io`] or [`Error::Config`] if config files cannot be read or written.
pub fn init_worktree_config(git_dir: &Path) -> Result<()> {
    let common_dir = common_git_dir_for_config(git_dir);
    let common_config_path = common_dir.join("config");
    let worktree_config_path = git_dir.join("config.worktree");

    if worktree_config_enabled(&common_dir) {
        if !worktree_config_path.exists() {
            if let Some(parent) = worktree_config_path.parent() {
                fs::create_dir_all(parent).map_err(Error::Io)?;
            }
            fs::write(&worktree_config_path, "").map_err(Error::Io)?;
        }
        return Ok(());
    }

    let mut common_cfg = open_or_create_config_file(&common_config_path, ConfigScope::Local)?;
    common_cfg.set("extensions.worktreeConfig", "true")?;

    let mut wt_cfg = open_or_create_config_file(&worktree_config_path, ConfigScope::Worktree)?;

    if config_file_bool_true(&common_cfg, "core.bare") {
        wt_cfg.set("core.bare", "true")?;
        common_cfg.unset("core.bare")?;
    }
    if let Some(worktree) = common_cfg.get("core.worktree") {
        wt_cfg.set("core.worktree", &worktree)?;
        common_cfg.unset("core.worktree")?;
    }

    common_cfg.write()?;
    wt_cfg.write()?;
    Ok(())
}

/// If the common `config` declares a repository format newer than Git's
/// `GIT_REPO_VERSION_READ`, return the human message Git prints for
/// `discover_git_directory_reason` / t1309.
pub fn early_config_ignore_repo_reason(common_dir: &Path) -> Option<String> {
    const GIT_REPO_VERSION_READ: u32 = 1;
    let path = common_dir.join("config");
    let content = fs::read_to_string(&path).ok()?;
    let mut version = 0u32;
    let mut in_core = false;
    for raw_line in content.lines() {
        let mut line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if line.starts_with('[') {
            let Some(end_idx) = line.find(']') else {
                continue;
            };
            let section = line[1..end_idx].trim();
            let section_name = section
                .split_whitespace()
                .next()
                .unwrap_or_default()
                .to_ascii_lowercase();
            in_core = section_name == "core";
            let remainder = line[end_idx + 1..].trim();
            if remainder.is_empty() || remainder.starts_with('#') || remainder.starts_with(';') {
                continue;
            }
            line = remainder;
        }
        if in_core {
            if let Some((key, value)) = line.split_once('=') {
                if key.trim().eq_ignore_ascii_case("repositoryformatversion") {
                    if let Ok(v) = value.trim().parse::<u32>() {
                        version = v;
                    }
                }
            }
        }
    }
    if version > GIT_REPO_VERSION_READ {
        Some(format!(
            "Expected git repo version <= {GIT_REPO_VERSION_READ}, found {version}"
        ))
    } else {
        None
    }
}

fn path_for_ceiling_compare(path: &Path) -> String {
    let path = path.to_string_lossy();
    #[cfg(windows)]
    {
        path.replace('\\', "/")
    }
    #[cfg(not(windows))]
    {
        path.into_owned()
    }
}

fn offset_1st_component(path: &str) -> usize {
    if path.starts_with('/') {
        1
    } else {
        0
    }
}

/// Git `longest_ancestor_length`: longest strict ancestor prefix among ceilings.
fn longest_ancestor_length(path: &str, ceilings: &[String]) -> Option<usize> {
    if path == "/" {
        return None;
    }
    let mut max_len: Option<usize> = None;
    for ceil in ceilings {
        let mut len = ceil.len();
        while len > 0 && ceil.as_bytes().get(len - 1) == Some(&b'/') {
            len -= 1;
        }
        if len == 0 {
            continue;
        }
        if path.len() <= len + 1 {
            continue;
        }
        if !path.starts_with(&ceil[..len]) {
            continue;
        }
        if path.as_bytes().get(len) != Some(&b'/') {
            continue;
        }
        if path.as_bytes().get(len + 1).is_none() {
            continue;
        }
        max_len = Some(max_len.map_or(len, |m| m.max(len)));
    }
    max_len
}

/// Determine the config file path for a repository or linked worktree.
fn repository_config_path(git_dir: &Path) -> Option<PathBuf> {
    let local = git_dir.join("config");
    if local.exists() {
        return Some(local);
    }
    let common = resolve_common_dir(git_dir)?;
    let shared = common.join("config");
    if shared.exists() {
        Some(shared)
    } else {
        None
    }
}

/// Validate core repository format/version compatibility.
///
/// Supports repository format versions 0 and 1, with extension handling that
/// matches Git's compatibility expectations in upstream repo-version tests.
/// Public wrapper for validate_repository_format.
pub fn validate_repo_format(git_dir: &Path) -> Result<()> {
    validate_repository_format(git_dir)
}

/// Validate parsed repository format (version and extensions) before using the repo.
pub(crate) fn validate_repository_format_parsed(parsed: &RepositoryFormat) -> Result<()> {
    if parsed.repo_version > 1 {
        return Err(Error::UnsupportedRepositoryFormatVersion(
            parsed.repo_version,
        ));
    }

    if let Some(raw) = parsed.ref_storage.as_deref() {
        crate::ref_storage::RefStorageFormat::parse_config_value(raw)?;
    }

    if let Some(msg) = parsed.format_error_message() {
        return Err(Error::Message(msg));
    }

    Ok(())
}

fn validate_repository_format(git_dir: &Path) -> Result<()> {
    let Some(config_path) = repository_config_path(git_dir) else {
        return Ok(());
    };
    let content = fs::read_to_string(&config_path).map_err(Error::Io)?;
    let parsed = parse_repository_format(&content, &config_path)?;
    validate_repository_format_parsed(&parsed)
}

/// The result of parsing `core.repositoryformatversion` and `extensions.*` from a
/// repository's `config` file, using Git-compatible format parsing.
pub(crate) struct RepositoryFormat {
    /// Declared `core.repositoryformatversion` (defaults to 0; invalid values ignored).
    pub(crate) repo_version: u32,
    /// All extension keys (lowercased) declared under `[extensions]`.
    pub(crate) extensions: BTreeSet<String>,
    /// Raw value of `extensions.refstorage`, if present.
    pub(crate) ref_storage: Option<String>,
}

/// Parse repository format from repository-local config (no global/system config).
///
/// # Errors
///
/// Returns [`Error::Io`] or [`Error::Config`] when config cannot be read or parsed.
pub(crate) fn read_repository_format_from_git_dir(git_dir: &Path) -> Result<RepositoryFormat> {
    let Some(config_path) = repository_config_path(git_dir) else {
        return Ok(RepositoryFormat {
            repo_version: 0,
            extensions: BTreeSet::new(),
            ref_storage: None,
        });
    };
    let content = fs::read_to_string(&config_path).map_err(Error::Io)?;
    parse_repository_format(&content, &config_path)
}

impl RepositoryFormat {
    /// Build git's `verify_repository_format` warning/error message for unsupported
    /// extension declarations, or `None` if the extensions are all acceptable.
    ///
    /// This does not cover the `core.repositoryformatversion > 1` case; callers that
    /// need the version message handle it separately via
    /// [`repository_format_warning`].
    fn format_error_message(&self) -> Option<String> {
        // Mirror git/setup.c `check_repo_format` / `verify_repository_format`. Extensions
        // split into:
        //   * v0-compatible (`handle_extension_v0`): respected even in a v0 repository.
        //   * v1-only (`handle_extension`): legal only when `core.repositoryformatversion >= 1`.
        // A v0 repository that declares any v1-only extension is rejected (t0001 #60, #62);
        // an unknown extension is rejected only in a v1 repository.
        let mut v1_only_found: Vec<&str> = Vec::new();
        let mut unknown_found: Vec<&str> = Vec::new();
        for extension in &self.extensions {
            match extension.as_str() {
                // v0-compatible extensions — always allowed.
                "noop" | "preciousobjects" | "partialclone" | "worktreeconfig" => {}
                // v1-only extensions — only valid with repository format version >= 1.
                "noop-v1"
                | "objectformat"
                | "compatobjectformat"
                | "refstorage"
                | "relativeworktrees"
                | "submodulepathconfig" => {
                    if self.repo_version == 0 {
                        v1_only_found.push(extension);
                    }
                }
                // Unknown extension — rejected only in a v1 repository.
                _ => {
                    if self.repo_version >= 1 {
                        unknown_found.push(extension);
                    }
                }
            }
        }

        if !unknown_found.is_empty() {
            let mut msg = if unknown_found.len() == 1 {
                "unknown repository extension found:".to_owned()
            } else {
                "unknown repository extensions found:".to_owned()
            };
            for ext in &unknown_found {
                msg.push_str(&format!("\n\t{ext}"));
            }
            return Some(msg);
        }

        if !v1_only_found.is_empty() {
            let mut msg = if v1_only_found.len() == 1 {
                "repo version is 0, but v1-only extension found:".to_owned()
            } else {
                "repo version is 0, but v1-only extensions found:".to_owned()
            };
            for ext in &v1_only_found {
                msg.push_str(&format!("\n\t{ext}"));
            }
            return Some(msg);
        }

        None
    }
}

/// Parse `core.repositoryformatversion` and `[extensions]` entries out of raw
/// `config` file contents.
///
/// # Errors
///
/// Returns [`Error::Config`] if a section header is malformed (no closing `]`).
fn parse_repository_format(content: &str, config_path: &Path) -> Result<RepositoryFormat> {
    let mut in_core = false;
    let mut in_extensions = false;
    let mut repo_version = 0u32;
    let mut extensions = BTreeSet::new();
    let mut ref_storage: Option<String> = None;

    for raw_line in content.lines() {
        let mut line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }

        if line.starts_with('[') {
            let Some(end_idx) = line.find(']') else {
                return Err(Error::Config(
                    format!("invalid config in {}", config_path.display()).into(),
                ));
            };

            let section = line[1..end_idx].trim();
            let section_name = section
                .split_whitespace()
                .next()
                .unwrap_or_default()
                .to_ascii_lowercase();
            in_core = section_name == "core";
            in_extensions = section_name == "extensions";

            let remainder = line[end_idx + 1..].trim();
            if remainder.is_empty() || remainder.starts_with('#') || remainder.starts_with(';') {
                continue;
            }
            line = remainder;
        }

        if in_core {
            if let Some((key, value)) = line.split_once('=') {
                if key.trim().eq_ignore_ascii_case("repositoryformatversion") {
                    // Match Git's `read_repository_format`: bad values are ignored (version stays 0).
                    if let Ok(v) = value.trim().parse::<u32>() {
                        repo_version = v;
                    }
                }
            }
        }

        if in_extensions {
            let (key, value) = if let Some((key, value)) = line.split_once('=') {
                (key.trim(), Some(value.trim()))
            } else {
                (line, None)
            };
            if key.eq_ignore_ascii_case("refstorage") {
                ref_storage = value.map(str::to_owned);
            }
            if !key.is_empty() {
                extensions.insert(key.to_ascii_lowercase());
            }
        }
    }

    Ok(RepositoryFormat {
        repo_version,
        extensions,
        ref_storage,
    })
}

/// Return the warning message git would print for a repository whose `config`
/// declares an unsupported format, or `None` if the format is acceptable.
///
/// This matches Git's repository-format verification behavior:
/// commands that run with `RUN_SETUP_GENTLY` (e.g. `git config`) emit this text as a
/// `warning:` and then behave as if no repository were present, which makes the command
/// fail with a non-zero exit. The returned string is the bare message without the
/// `warning: ` prefix.
///
/// `git_dir` is the resolved git directory; its `config` (or the common-dir `config` for
/// linked worktrees) is parsed. A missing config yields `None` (git treats it as ok).
///
/// # Errors
///
/// Returns [`Error::Io`] if the config file exists but cannot be read, or
/// [`Error::Config`] if a section header is malformed.
pub fn repository_format_warning(git_dir: &Path) -> Result<Option<String>> {
    const GIT_REPO_VERSION_READ: u32 = 1;
    let Some(config_path) = repository_config_path(git_dir) else {
        return Ok(None);
    };
    let content = fs::read_to_string(&config_path).map_err(Error::Io)?;
    let parsed = parse_repository_format(&content, &config_path)?;

    if parsed.repo_version > GIT_REPO_VERSION_READ {
        return Ok(Some(format!(
            "Expected git repo version <= {GIT_REPO_VERSION_READ}, found {}",
            parsed.repo_version
        )));
    }

    Ok(parsed.format_error_message())
}

/// Try to open a repository rooted exactly at `dir`.
///
/// Returns `Ok(None)` when `dir` is not a repository root (the caller should
/// walk up); returns `Err` on a structural problem.
/// Result of probing a single directory during [`Repository::discover`].
struct DiscoveredAt {
    repo: Repository,
    /// When discovery used a `.git` gitfile, the path to that file (for ownership checks).
    gitfile: Option<PathBuf>,
}

fn try_open_at(options: &RepositoryOptions, dir: &Path) -> Result<Option<DiscoveredAt>> {
    let env = &options.environment;
    let dot_git = dir.join(".git");

    // Check for special file types (FIFO, socket, etc.) — reject them
    // instead of walking up to a parent repository.
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileTypeExt;
        if let Ok(meta) = fs::symlink_metadata(&dot_git) {
            let ft = meta.file_type();
            if ft.is_fifo() || ft.is_socket() || ft.is_block_device() || ft.is_char_device() {
                return Err(Error::NotARepository(format!(
                    "invalid gitfile format: {} is not a regular file",
                    dot_git.display()
                )));
            }
            if ft.is_symlink() {
                if let Ok(target_meta) = fs::metadata(&dot_git) {
                    let tft = target_meta.file_type();
                    if tft.is_fifo()
                        || tft.is_socket()
                        || tft.is_block_device()
                        || tft.is_char_device()
                    {
                        return Err(Error::NotARepository(format!(
                            "invalid gitfile format: {} is not a regular file",
                            dot_git.display()
                        )));
                    }
                }
            }
        }
    }

    if dot_git.is_file() {
        // gitfile indirection: file contains "gitdir: <path>"
        let content =
            fs::read_to_string(&dot_git).map_err(|e| Error::NotARepository(e.to_string()))?;
        let git_dir = parse_gitfile(&content, dir)?;
        let mut repo =
            Repository::open_skipping_format_validation_with_options(options, &git_dir, Some(dir))?;
        // Linked worktree: `core.worktree` in the common config may point at another directory
        // (t1501). When the process cwd is not inside that configured tree, Git uses the
        // discovery directory as the work tree (commondir overrides for ops under the real tree).
        if resolve_common_dir(&git_dir).is_some() {
            let cwd = env.discovery_cwd();
            if repo.work_tree.is_some() && !is_inside_work_tree(&repo, &cwd) {
                let root = if dir.is_absolute() {
                    dir.to_path_buf()
                } else {
                    cwd.join(dir)
                };
                repo.work_tree = Some(root.canonicalize().unwrap_or(root));
            }
        }
        let root = if dir.is_absolute() {
            dir.to_path_buf()
        } else {
            env.discovery_cwd().join(dir)
        };
        repo.discovery_root = Some(root.canonicalize().unwrap_or(root));
        repo.discovery_via_gitfile = true;
        return Ok(Some(DiscoveredAt {
            repo,
            gitfile: Some(dot_git.clone()),
        }));
    }

    if dot_git.is_dir() {
        // If .git is a symlink to a directory, resolve the symlink target
        // for validation but keep the original .git path for user-facing output
        // (matches real git behavior: `rev-parse --git-dir` shows `.git`).
        let open_path = if dot_git.is_symlink() {
            // Resolve the symlink target for validation
            dot_git.read_link().unwrap_or_else(|_| dot_git.clone())
        } else {
            dot_git.clone()
        };
        // Try to open; if the directory is empty or invalid, continue
        // walking up (e.g. an empty .git/ directory should be ignored).
        match Repository::open_skipping_format_validation_with_options(
            options,
            &open_path,
            Some(dir),
        ) {
            Ok(mut repo) => {
                // Restore the original path so rev-parse shows .git not the
                // resolved symlink target.
                if dot_git.is_symlink() {
                    let abs_dot_git = if dot_git.is_absolute() {
                        dot_git
                    } else {
                        dir.join(".git")
                    };
                    repo.git_dir = abs_dot_git;
                }
                let root = if dir.is_absolute() {
                    dir.to_path_buf()
                } else {
                    env.discovery_cwd().join(dir)
                };
                repo.discovery_root = Some(root.canonicalize().unwrap_or(root));
                repo.discovery_via_gitfile = false;
                return Ok(Some(DiscoveredAt {
                    repo,
                    gitfile: None,
                }));
            }
            Err(Error::NotARepository(_)) | Err(Error::Config(_)) => return Ok(None),
            Err(Error::Message(ref msg)) if msg.contains("bad config") => return Ok(None),
            Err(e) => return Err(e),
        }
    }

    // Linked-worktree gitdir/admin directories contain HEAD and commondir,
    // and can be opened as repositories even without a local objects/ dir.
    if dir.join("HEAD").is_file() && dir.join("commondir").is_file() {
        maybe_trace_implicit_bare_repository(dir, env);
        let repo = Repository::open_with(options, dir, None)?;
        return Ok(Some(DiscoveredAt {
            repo,
            gitfile: None,
        }));
    }

    // Check if `dir` itself is a bare repo (has objects/ and HEAD directly)
    if dir.join("objects").is_dir() && dir.join("HEAD").is_file() {
        maybe_trace_implicit_bare_repository(dir, env);
        // Check safe.bareRepository policy before opening bare repos.
        // When set to "explicit", implicit bare repo discovery is forbidden
        // unless GIT_DIR was set (handled earlier in discover()).
        if !is_inside_dot_git(dir) {
            if let Ok(cfg) = crate::config::ConfigSet::load(env, None, true) {
                if let Some(val) = cfg.get("safe.bareRepository") {
                    if val.eq_ignore_ascii_case("explicit") {
                        return Err(Error::ForbiddenBareRepository(dir.display().to_string()));
                    }
                }
            }
        }
        let repo = Repository::open_with(options, dir, None)?;
        return Ok(Some(DiscoveredAt {
            repo,
            gitfile: None,
        }));
    }

    Ok(None)
}

fn is_inside_dot_git(path: &Path) -> bool {
    path.components().any(|c| c.as_os_str() == ".git")
}

fn maybe_trace_implicit_bare_repository(dir: &Path, environment: &Environment) {
    let Some(path) = environment
        .git_trace2_perf
        .as_deref()
        .filter(|p| !p.is_empty())
    else {
        return;
    };

    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(file, "setup: implicit-bare-repository:{}", dir.display());
    }
}

/// Collect effective `safe.directory` values from protected config (system/global/command),
/// applying empty-value resets like Git.
fn safe_directory_effective_values(git_dir: &Path, env: &Environment) -> Vec<String> {
    let cfg = crate::config::ConfigSet::load(env, Some(git_dir), true)
        .unwrap_or_else(|_| crate::config::ConfigSet::new());
    let mut values: Vec<String> = Vec::new();
    for e in cfg.entries() {
        if e.key == "safe.directory"
            && e.scope != crate::config::ConfigScope::Local
            && e.scope != crate::config::ConfigScope::Worktree
        {
            values.push(e.value.clone().unwrap_or_else(|| "true".to_owned()));
        }
    }
    let mut effective: Vec<String> = Vec::new();
    for v in values {
        if v.is_empty() {
            effective.clear();
        } else {
            effective.push(v);
        }
    }
    effective
}

fn ensure_safe_directory_allows(
    git_dir: &Path,
    checked: &Path,
    environment: &Environment,
) -> Result<()> {
    let effective = safe_directory_effective_values(git_dir, environment);
    let checked_s = checked.to_string_lossy().to_string();
    let _ = environment.grit_debug_safe_dir;
    if effective
        .iter()
        .any(|v| safe_directory_matches(v, &checked_s, &environment.discovery_cwd()))
    {
        return Ok(());
    }
    Err(Error::DubiousOwnership(checked_s))
}

#[cfg(unix)]
fn path_lstat_uid(path: &Path) -> std::io::Result<u32> {
    use std::os::unix::fs::MetadataExt;
    let meta = fs::symlink_metadata(path)?;
    Ok(meta.uid())
}

/// Match Git's `ensure_valid_ownership`: check gitfile, worktree, and gitdir ownership,
/// then `safe.directory` when any path is not owned by the effective user.
#[cfg(unix)]
fn ensure_valid_ownership(
    environment: &Environment,
    gitfile: Option<&Path>,
    worktree: Option<&Path>,
    gitdir: &Path,
) -> Result<()> {
    const ROOT_UID: u32 = 0;
    let sudo_euid = environment
        .sudo_uid
        .as_deref()
        .and_then(|s| s.parse::<u32>().ok());

    fn owned_by_effective_user(path: &Path, sudo_euid: Option<u32>) -> std::io::Result<bool> {
        let st_uid = path_lstat_uid(path)?;
        let mut euid = nix::unistd::geteuid().as_raw();
        if euid == ROOT_UID {
            if st_uid == ROOT_UID {
                return Ok(true);
            }
            if let Some(sudo_uid) = sudo_euid {
                euid = sudo_uid;
            }
        }
        Ok(st_uid == euid)
    }

    let assume_different = environment.test_assume_different_owner();
    if !assume_different {
        let gitfile_ok = gitfile
            .map(|p| owned_by_effective_user(p, sudo_euid))
            .transpose()?
            .unwrap_or(true);
        // Git may use a `GIT_WORK_TREE` that does not exist yet (t1510); skip ownership when
        // the path is absent instead of failing discovery with ENOENT.
        let wt_ok = match worktree {
            None => true,
            Some(wt) => match owned_by_effective_user(wt, sudo_euid) {
                Ok(ok) => ok,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => true,
                Err(e) => return Err(Error::Io(e)),
            },
        };
        let gd_ok = owned_by_effective_user(gitdir, sudo_euid)?;
        if gitfile_ok && wt_ok && gd_ok {
            return Ok(());
        }
    }

    let data_path = if let Some(wt) = worktree {
        wt.canonicalize().unwrap_or_else(|_| wt.to_path_buf())
    } else {
        gitdir
            .canonicalize()
            .unwrap_or_else(|_| gitdir.to_path_buf())
    };
    ensure_safe_directory_allows(gitdir, &data_path, environment)
}

#[cfg(not(unix))]
fn ensure_valid_ownership(
    _environment: &Environment,
    _gitfile: Option<&Path>,
    _worktree: Option<&Path>,
    _gitdir: &Path,
) -> Result<()> {
    Ok(())
}

impl Repository {
    /// Enforce `safe.directory` ownership checks, matching upstream behavior.
    ///
    /// When `GIT_TEST_ASSUME_DIFFERENT_OWNER=1`, ownership is considered unsafe
    /// unless a matching `safe.directory` value is configured in system/global/
    /// command scopes (repository-local config is ignored).
    pub fn enforce_safe_directory(&self) -> Result<()> {
        let assume_different =
            self.test_assume_different_owner || self.environment.test_assume_different_owner();
        if !assume_different {
            return Ok(());
        }

        if self.explicit_git_dir {
            return Ok(());
        }

        // In normal discovery, ownership is checked against worktree paths
        // unless invocation starts inside the gitdir, in which case gitdir is
        // checked.
        let checked = if let Some(wt) = &self.work_tree {
            let cwd = Some(self.environment.discovery_cwd());
            if let Some(cwd) = cwd {
                if cwd
                    .canonicalize()
                    .ok()
                    .is_some_and(|c| c.starts_with(&self.git_dir))
                {
                    self.git_dir
                        .canonicalize()
                        .unwrap_or_else(|_| self.git_dir.clone())
                } else {
                    wt.canonicalize().unwrap_or_else(|_| wt.clone())
                }
            } else {
                wt.canonicalize().unwrap_or_else(|_| wt.clone())
            }
        } else {
            self.git_dir
                .canonicalize()
                .unwrap_or_else(|_| self.git_dir.clone())
        };

        let _ = self.environment.grit_debug_safe_dir;
        self.enforce_safe_directory_checked(&checked)
    }

    /// Enforce safe.directory checks using the repository git-dir path.
    ///
    /// Used by operations that explicitly open another repository by path
    /// (e.g. local clone source).
    pub fn enforce_safe_directory_git_dir(&self) -> Result<()> {
        let assume_different =
            self.test_assume_different_owner || self.environment.test_assume_different_owner();
        if !assume_different {
            return Ok(());
        }
        let checked = self
            .git_dir
            .canonicalize()
            .unwrap_or_else(|_| self.git_dir.clone());
        let _ = self.environment.grit_debug_safe_dir;
        self.enforce_safe_directory_checked(&checked)
    }

    /// Enforce safe.directory checks against an explicit checked path.
    pub fn enforce_safe_directory_git_dir_with_path(&self, checked: &Path) -> Result<()> {
        let assume_different =
            self.test_assume_different_owner || self.environment.test_assume_different_owner();
        if !assume_different {
            return Ok(());
        }
        self.enforce_safe_directory_checked(checked)
    }

    fn enforce_safe_directory_checked(&self, checked: &Path) -> Result<()> {
        ensure_safe_directory_allows(&self.git_dir, checked, self.environment.as_ref())
    }

    /// Verify the repository is safe to use as a `git clone` source (local clone).
    ///
    /// When `GIT_TEST_ASSUME_DIFFERENT_OWNER` is set, applies the same `safe.directory`
    /// rules as discovery. Otherwise checks filesystem ownership of the git directory
    /// only (matching Git's `die_upon_dubious_ownership` for clone).
    pub fn verify_safe_for_clone_source(&self) -> Result<()> {
        let assume_different =
            self.test_assume_different_owner || self.environment.test_assume_different_owner();
        if assume_different {
            self.enforce_safe_directory_git_dir()
        } else {
            #[cfg(unix)]
            {
                ensure_valid_ownership(self.environment.as_ref(), None, None, &self.git_dir)
            }
            #[cfg(not(unix))]
            {
                Ok(())
            }
        }
    }
}

fn normalize_fs_path(raw: &str) -> String {
    use std::path::Component;
    let p = std::path::Path::new(raw);
    let mut parts: Vec<String> = Vec::new();
    let mut absolute = false;
    for c in p.components() {
        match c {
            Component::RootDir => {
                absolute = true;
                parts.clear();
            }
            Component::CurDir => {}
            Component::ParentDir => {
                if !parts.is_empty() {
                    parts.pop();
                }
            }
            Component::Normal(s) => parts.push(s.to_string_lossy().to_string()),
            Component::Prefix(_) => {}
        }
    }
    let mut out = if absolute {
        String::from("/")
    } else {
        String::new()
    };
    out.push_str(&parts.join("/"));
    out
}

fn safe_directory_matches(config_value: &str, checked: &str, cwd: &Path) -> bool {
    if config_value == "*" {
        return true;
    }
    if config_value == "." {
        // CWD only.
        let cwd_s = normalize_fs_path(&cwd.to_string_lossy());
        let checked_s = normalize_fs_path(checked);
        return cwd_s == checked_s;
    }

    let canonicalize_or_normalize = |raw: &str| -> String {
        let p = std::path::Path::new(raw);
        if p.exists() {
            p.canonicalize()
                .map(|c| c.to_string_lossy().to_string())
                .map(|s| normalize_fs_path(&s))
                .unwrap_or_else(|_| normalize_fs_path(raw))
        } else {
            normalize_fs_path(raw)
        }
    };

    let config_norm = canonicalize_or_normalize(config_value);
    let checked_norm = normalize_fs_path(checked);

    if config_norm.ends_with("/*") {
        let prefix_raw = &config_norm[..config_norm.len() - 2];
        let prefix_norm = canonicalize_or_normalize(prefix_raw);
        let mut prefix = prefix_norm;
        if !prefix.ends_with('/') {
            prefix.push('/');
        }
        return checked_norm.starts_with(&prefix);
    }

    config_norm == checked_norm
}

fn warn_core_bare_worktree_conflict(options: &RepositoryOptions, git_dir: &Path) {
    if options
        .environment
        .git_work_tree
        .as_deref()
        .is_some_and(|s| !s.trim().is_empty())
    {
        return;
    }
    if let Ok((bare, wt)) = read_core_bare_and_worktree(git_dir) {
        if bare && wt.is_some() {
            options
                .diagnostics
                .warn(diagnostics::Warning::CoreBareWithWorktree);
        }
    }
}

fn read_core_bare_and_worktree_from_config(cfg: &ConfigSet) -> (bool, Option<String>) {
    let bare = cfg
        .get_bool("core.bare")
        .and_then(|r| r.ok())
        .unwrap_or(false);
    let worktree = cfg.get("core.worktree").map(|s| s.to_owned());
    (bare, worktree)
}

fn read_core_bare_and_worktree(git_dir: &Path) -> Result<(bool, Option<String>)> {
    let cfg = ConfigSet::load(
        &crate::environment::Environment::empty(),
        Some(git_dir),
        true,
    )
    .unwrap_or_default();
    Ok(read_core_bare_and_worktree_from_config(&cfg))
}

/// Reject impossible `GIT_WORK_TREE` values before repository setup (matches Git's
/// `validate_worktree` / `die` on bogus absolute paths, e.g. t1501).
fn validate_git_work_tree_path(path: &Path) -> Result<()> {
    if !path.is_absolute() {
        return Ok(());
    }
    let comps: Vec<Component<'_>> = path.components().collect();
    let Some(last_normal_idx) = comps
        .iter()
        .enumerate()
        .rev()
        .find_map(|(i, c)| matches!(c, Component::Normal(_)).then_some(i))
    else {
        return Ok(());
    };
    let mut cur = PathBuf::new();
    for (i, comp) in comps.iter().enumerate() {
        match comp {
            Component::Prefix(p) => cur.push(p.as_os_str()),
            Component::RootDir => cur.push(comp.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                let _ = cur.pop();
            }
            Component::Normal(seg) => {
                cur.push(seg);
                if i != last_normal_idx && !cur.exists() {
                    return Err(Error::PathError(format!(
                        "Invalid path '{}': No such file or directory",
                        cur.display()
                    )));
                }
            }
        }
    }
    Ok(())
}

fn resolve_core_worktree_path(git_dir: &Path, raw: &str) -> Result<PathBuf> {
    let p = Path::new(raw);
    if p.is_absolute() {
        return Ok(p.canonicalize().unwrap_or_else(|_| p.to_path_buf()));
    }
    let joined = git_dir.join(p);
    Ok(joined.canonicalize().unwrap_or(joined))
}

/// When `GIT_DIR` names a gitfile, resolve to the real git directory.
fn resolve_git_dir_env_path(git_dir: &Path) -> Result<PathBuf> {
    if git_dir.is_file() {
        let content =
            fs::read_to_string(git_dir).map_err(|e| Error::NotARepository(e.to_string()))?;
        let base = git_dir
            .parent()
            .ok_or_else(|| Error::NotARepository(git_dir.display().to_string()))?;
        return parse_gitfile(&content, base);
    }
    Ok(git_dir.to_path_buf())
}

/// Resolve an explicit git directory path the same way as `GIT_DIR` (including gitfile indirection).
///
/// # Errors
///
/// Returns [`Error::NotARepository`] for invalid gitfile content.
pub fn resolve_git_directory_arg(git_dir: &Path) -> Result<PathBuf> {
    resolve_git_dir_env_path(git_dir)
}

/// Resolves a work tree's `.git` path (directory or gitfile) to the real git directory.
///
/// # Errors
///
/// Returns [`Error::NotARepository`] when `.git` is missing, invalid, or the gitfile target is absent.
pub fn resolve_dot_git(dot_git: &Path) -> Result<PathBuf> {
    if dot_git.is_dir() {
        return dot_git
            .canonicalize()
            .map_err(|_| Error::NotARepository(dot_git.display().to_string()));
    }
    if dot_git.is_file() {
        let content =
            fs::read_to_string(dot_git).map_err(|e| Error::NotARepository(e.to_string()))?;
        let base = dot_git
            .parent()
            .ok_or_else(|| Error::NotARepository(dot_git.display().to_string()))?;
        return parse_gitfile(&content, base);
    }
    Err(Error::NotARepository(dot_git.display().to_string()))
}

/// Parse a gitfile's `"gitdir: <path>"` line.
fn parse_gitfile(content: &str, base: &Path) -> Result<PathBuf> {
    for line in content.lines() {
        if let Some(rest) = line.strip_prefix("gitdir:") {
            let rel = rest.trim();
            let path = if Path::new(rel).is_absolute() {
                PathBuf::from(rel)
            } else {
                base.join(rel)
            };
            if !path.exists() {
                return Err(Error::NotARepository(path.display().to_string()));
            }
            return Ok(path);
        }
    }
    Err(Error::NotARepository("invalid gitfile format".to_owned()))
}

/// Initialise a new Git repository at the given path.
///
/// Creates the standard directory skeleton (objects/, refs/heads/, refs/tags/,
/// info/, hooks/) and a default `HEAD` pointing to `refs/heads/<initial_branch>`.
///
/// # Parameters
///
/// - `path` — root directory to initialise (created if absent).
/// - `bare` — if true, `path` itself becomes the git-dir; otherwise `path/.git`.
/// - `initial_branch` — branch name for `HEAD` (e.g. `"main"`).
/// - `template_dir` — optional template directory; if `None`, a minimal skeleton
///   is created.
///
/// # Errors
///
/// Ensure standard git directory layout without overwriting `HEAD`, `config`, or refs.
fn ensure_git_directory_layout(
    git_dir: &Path,
    bare: bool,
    ref_storage: crate::ref_storage::RefStorageFormat,
    skip_hooks_and_info: bool,
) -> Result<()> {
    let mut subs = vec![
        "objects",
        "objects/info",
        "objects/pack",
        "refs",
        "refs/heads",
        "refs/tags",
    ];
    if !bare && !skip_hooks_and_info {
        subs.push("info");
        subs.push("hooks");
    }
    for sub in subs {
        fs::create_dir_all(git_dir.join(sub))?;
    }

    if ref_storage.is_reftable() {
        let reftable_dir = git_dir.join("reftable");
        fs::create_dir_all(&reftable_dir)?;
        let tables_list = reftable_dir.join("tables.list");
        if !tables_list.exists() {
            fs::write(&tables_list, "")?;
        }
    }
    Ok(())
}

fn write_fresh_git_directory(
    git_dir: &Path,
    bare: bool,
    initial_branch: &str,
    template_dir: Option<&Path>,
    ref_storage: crate::ref_storage::RefStorageFormat,
    skip_hooks_and_info: bool,
) -> Result<()> {
    ensure_git_directory_layout(git_dir, bare, ref_storage, skip_hooks_and_info)?;

    if let Some(tmpl) = template_dir {
        if tmpl.is_dir() {
            copy_template(tmpl, git_dir)?;
        }
    }

    let head_content = format!("ref: refs/heads/{initial_branch}\n");
    fs::write(git_dir.join("HEAD"), head_content)?;

    let needs_extensions = ref_storage.is_reftable();
    let repo_version = if needs_extensions { 1 } else { 0 };

    let mut config_content = String::from("[core]\n");
    config_content.push_str(&format!("\trepositoryformatversion = {repo_version}\n"));
    config_content.push_str("\tfilemode = true\n");
    if bare {
        config_content.push_str("\tbare = true\n");
    } else {
        config_content.push_str("\tbare = false\n");
        config_content.push_str("\tlogallrefupdates = true\n");
    }
    if needs_extensions {
        config_content.push_str("[extensions]\n");
        config_content.push_str("\trefStorage = reftable\n");
    }
    fs::write(git_dir.join("config"), config_content)?;

    // Merge `config` from the template on top of the default (matches `git clone --template`).
    if let Some(tmpl) = template_dir {
        if tmpl.is_dir() {
            let tmpl_config = tmpl.join("config");
            if tmpl_config.is_file() {
                let tmpl_text = fs::read_to_string(&tmpl_config)?;
                let tmpl_parsed = ConfigFile::parse(&tmpl_config, &tmpl_text, ConfigScope::Local)?;
                let dest_path = git_dir.join("config");
                let dest_text = fs::read_to_string(&dest_path)?;
                let mut dest_parsed =
                    ConfigFile::parse(&dest_path, &dest_text, ConfigScope::Local)?;
                for e in &tmpl_parsed.entries {
                    // Git clone ignores `core.bare` from templates (non-bare clone must stay non-bare).
                    if e.key == "core.bare" {
                        continue;
                    }
                    if let Some(v) = &e.value {
                        let _ = dest_parsed.set(&e.key, v);
                    } else {
                        let _ = dest_parsed.set(&e.key, "true");
                    }
                }
                dest_parsed.write()?;
            }
        }
    }

    apply_init_filesystem_config(
        git_dir,
        InitFilesystemConfigOptions::default(),
        &Environment::empty(),
    )?;

    fs::write(
        git_dir.join("description"),
        "Unnamed repository; edit this file 'description' to name the repository.\n",
    )?;
    Ok(())
}

/// Initialise a non-bare repository with the git directory at `git_dir` and the work tree at `work_tree`.
///
/// Creates `work_tree/.git` as a gitfile pointing at `git_dir` (absolute path). Matches `git clone
/// --separate-git-dir` layout.
///
/// # Errors
///
/// Returns [`Error::Io`] on filesystem failures.
pub fn init_repository_separate_git_dir(
    work_tree: &Path,
    git_dir: &Path,
    initial_branch: &str,
    template_dir: Option<&Path>,
    ref_storage: crate::ref_storage::RefStorageFormat,
) -> Result<Repository> {
    let skip_hooks_info = template_dir.is_some_and(|p| p.as_os_str().is_empty());
    fs::create_dir_all(work_tree)?;
    fs::create_dir_all(git_dir)?;
    write_fresh_git_directory(
        git_dir,
        false,
        initial_branch,
        template_dir,
        ref_storage,
        skip_hooks_info,
    )?;

    // Write an absolute `gitdir:` path, matching C Git's `init_db` →
    // `set_git_dir(real_git_dir, make_realpath=1)` → `separate_git_dir`, which records the
    // realpath of the separate git directory (`t5601` "clone separate gitdir: output"). This
    // path is only used by `git clone --separate-git-dir`; submodule layouts use a different
    // code path and are unaffected.
    let gitfile = work_tree.join(".git");
    let abs_git_dir = fs::canonicalize(git_dir).unwrap_or_else(|_| git_dir.to_path_buf());
    let abs_git_dir = abs_git_dir.to_string_lossy().replace('\\', "/");
    fs::write(gitfile, format!("gitdir: {abs_git_dir}\n"))?;

    Repository::open(git_dir, Some(work_tree))
}

/// Initialise a **minimal** bare repository directory layout matching `git clone --template= --bare`.
///
/// Git's clone-with-empty-template omits `hooks/`, `info/`, `description`, and `branches/` until
/// something needs them; tests rely on `mkdir <repo>/info` succeeding afterward.
///
/// # Parameters
///
/// - `git_dir` — bare repository root (the destination `.git` directory for a bare clone).
/// - `initial_branch` — used only for the initial `HEAD` symref text before clone rewires it.
///
/// # Errors
///
/// Returns [`Error::Io`] on filesystem failures.
/// Ensure `core.bare = true` in the repository `config` (used after `git clone --bare`).
pub fn ensure_core_bare(git_dir: &Path) -> Result<()> {
    let path = git_dir.join("config");
    let text = fs::read_to_string(&path).unwrap_or_default();
    if text.lines().any(|l| {
        let t = l.trim();
        t == "bare = true" || t == "bare=true"
    }) {
        return Ok(());
    }
    let mut out = text;
    if !out.ends_with('\n') && !out.is_empty() {
        out.push('\n');
    }
    if !out.contains("[core]") {
        out.push_str("[core]\n");
    }
    out.push_str("\tbare = true\n");
    fs::write(path, out).map_err(Error::Io)
}

pub fn init_bare_clone_minimal(
    git_dir: &Path,
    initial_branch: &str,
    ref_storage: crate::ref_storage::RefStorageFormat,
) -> Result<()> {
    for sub in &[
        "objects",
        "objects/info",
        "objects/pack",
        "refs",
        "refs/heads",
        "refs/tags",
    ] {
        fs::create_dir_all(git_dir.join(sub))?;
    }

    if ref_storage.is_reftable() {
        let reftable_dir = git_dir.join("reftable");
        fs::create_dir_all(&reftable_dir)?;
        let tables_list = reftable_dir.join("tables.list");
        if !tables_list.exists() {
            fs::write(&tables_list, "")?;
        }
    }

    let head_content = format!("ref: refs/heads/{initial_branch}\n");
    fs::write(git_dir.join("HEAD"), head_content)?;

    let needs_extensions = ref_storage.is_reftable();
    let repo_version = if needs_extensions { 1 } else { 0 };
    let mut config_content = String::from("[core]\n");
    config_content.push_str(&format!("\trepositoryformatversion = {repo_version}\n"));
    config_content.push_str("\tfilemode = true\n");
    config_content.push_str("\tbare = true\n");
    if needs_extensions {
        config_content.push_str("[extensions]\n");
        config_content.push_str("\trefStorage = reftable\n");
    }
    fs::write(git_dir.join("config"), config_content)?;
    apply_init_filesystem_config(
        git_dir,
        InitFilesystemConfigOptions::default(),
        &crate::environment::Environment::empty(),
    )?;

    fs::write(
        git_dir.join("packed-refs"),
        "# pack-refs with: peeled fully-peeled sorted\n",
    )?;
    Ok(())
}

pub fn init_repository(
    path: &Path,
    bare: bool,
    initial_branch: &str,
    template_dir: Option<&Path>,
    ref_storage: crate::ref_storage::RefStorageFormat,
) -> Result<Repository> {
    let skip_hooks_info = !bare && template_dir.is_some_and(|p| p.as_os_str().is_empty());
    let git_dir = if bare {
        path.to_path_buf()
    } else {
        path.join(".git")
    };

    if !bare {
        fs::create_dir_all(path)?;
    }
    fs::create_dir_all(&git_dir)?;
    let existing = git_dir.join("config").is_file() && git_dir.join("HEAD").is_file();
    if existing {
        ensure_git_directory_layout(&git_dir, bare, ref_storage, skip_hooks_info)?;
    } else {
        write_fresh_git_directory(
            &git_dir,
            bare,
            initial_branch,
            template_dir,
            ref_storage,
            skip_hooks_info,
        )?;
    }

    let work_tree = if bare { None } else { Some(path) };
    Repository::open(&git_dir, work_tree)
}

/// Initialise a **bare** repository at `git_dir` with `core.worktree` set to `work_tree`.
///
/// Used when `GIT_WORK_TREE` is set during `git clone`: the clone destination is the bare
/// git directory and checked-out files go under the environment work tree (matches upstream Git).
///
/// # Errors
///
/// Returns [`Error::Io`] on filesystem failures.
pub fn init_bare_with_env_worktree(
    git_dir: &Path,
    work_tree: &Path,
    initial_branch: &str,
    template_dir: Option<&Path>,
    ref_storage: crate::ref_storage::RefStorageFormat,
) -> Result<Repository> {
    fs::create_dir_all(git_dir)?;
    fs::create_dir_all(work_tree)?;
    write_fresh_git_directory(
        git_dir,
        true,
        initial_branch,
        template_dir,
        ref_storage,
        false,
    )?;
    let work_tree_abs = fs::canonicalize(work_tree).unwrap_or_else(|_| work_tree.to_path_buf());
    let config_path = git_dir.join("config");
    let mut config = match ConfigFile::from_path(&config_path, ConfigScope::Local)? {
        Some(c) => c,
        None => ConfigFile::parse(&config_path, "", ConfigScope::Local)?,
    };
    config.set("core.worktree", &work_tree_abs.to_string_lossy())?;
    config.write()?;
    Repository::open(git_dir, Some(work_tree))
}

/// Initialise a repository whose git directory is separate from the work tree.
///
/// Creates `git_dir` with the usual layout, writes `work_tree/.git` as a gitfile
/// pointing at `git_dir`, and sets `core.worktree` in `git_dir/config`.
pub fn init_repository_separate(
    work_tree: &Path,
    git_dir: &Path,
    initial_branch: &str,
    template_dir: Option<&Path>,
) -> Result<Repository> {
    fs::create_dir_all(work_tree)?;
    if git_dir.exists() {
        return Err(Error::PathError(format!(
            "git directory '{}' already exists",
            git_dir.display()
        )));
    }

    for sub in &[
        "objects",
        "objects/info",
        "objects/pack",
        "refs",
        "refs/heads",
        "refs/tags",
        "info",
        "hooks",
    ] {
        fs::create_dir_all(git_dir.join(sub))?;
    }

    if let Some(tmpl) = template_dir {
        if tmpl.is_dir() {
            copy_template(tmpl, git_dir)?;
        }
    }

    fs::write(
        git_dir.join("HEAD"),
        format!("ref: refs/heads/{initial_branch}\n"),
    )?;

    let work_tree_abs = fs::canonicalize(work_tree).unwrap_or_else(|_| work_tree.to_path_buf());
    let git_dir_abs = fs::canonicalize(git_dir).unwrap_or_else(|_| git_dir.to_path_buf());
    let config_content = format!(
        "[core]\n\trepositoryformatversion = 0\n\tfilemode = true\n\tbare = false\n\tlogallrefupdates = true\n\tworktree = {}\n",
        work_tree_abs.display()
    );
    fs::write(git_dir.join("config"), config_content)?;
    apply_init_filesystem_config(
        git_dir,
        InitFilesystemConfigOptions::default(),
        &Environment::empty(),
    )?;
    fs::write(
        git_dir.join("description"),
        "Unnamed repository; edit this file 'description' to name the repository.\n",
    )?;

    let gitfile = work_tree.join(".git");
    fs::write(&gitfile, format!("gitdir: {}\n", git_dir_abs.display()))?;

    Repository::open(git_dir, Some(work_tree))
}

/// Recursively copy template files from `src` to `dst`.
fn copy_template(src: &Path, dst: &Path) -> Result<()> {
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        if src_path.is_dir() {
            fs::create_dir_all(&dst_path)?;
            copy_template(&src_path, &dst_path)?;
        } else {
            fs::copy(&src_path, &dst_path)?;
        }
    }
    Ok(())
}

/// Parse `GIT_CEILING_DIRECTORIES` into a list of absolute paths and whether
/// symlink resolution should be skipped.
///
/// The variable is colon-separated (`:`) on Unix.  Empty entries and
/// non-absolute paths are silently skipped, matching Git's behaviour.
///
/// A leading colon (`:path1:path2`) disables symlink resolution for all
/// ceiling paths AND the cwd used for comparison (Git `resolve_symlinks` flag).
fn parse_ceiling_directories(env: &Environment) -> (Vec<PathBuf>, bool) {
    let Some(raw) = env.git_ceiling_directories.as_deref() else {
        return (Vec::new(), false);
    };
    if raw.is_empty() {
        return (Vec::new(), false);
    }
    // A leading colon means "don't resolve symlinks".
    let (no_resolve, effective) = match raw.strip_prefix(':') {
        Some(rest) => (true, rest),
        None => (false, raw),
    };
    let paths = effective
        .split(':')
        .filter(|s| !s.is_empty())
        .filter_map(|s| {
            let p = PathBuf::from(s);
            if !p.is_absolute() {
                return None;
            }
            if no_resolve {
                // Strip trailing slashes for consistent comparison but don't resolve symlinks.
                let s = s.trim_end_matches('/');
                Some(PathBuf::from(s))
            } else {
                // Canonicalize to resolve symlinks; fall back to the raw path
                // (with trailing slashes stripped) when the directory doesn't exist.
                Some(p.canonicalize().unwrap_or_else(|_| {
                    let s = s.trim_end_matches('/');
                    PathBuf::from(s)
                }))
            }
        })
        .collect();
    (paths, no_resolve)
}

/// Validate the repository format version from config text.
/// Returns Ok if the format is supported, Err with message if not.
pub fn validate_repo_config(config_text: &str) -> std::result::Result<(), String> {
    let mut version: u32 = 0;
    let mut in_core = false;
    for line in config_text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_core = trimmed.to_lowercase().starts_with("[core");
            continue;
        }
        if in_core {
            if let Some(rest) = trimmed.strip_prefix("repositoryformatversion") {
                let val = rest.trim_start_matches([' ', '=']).trim();
                if let Ok(v) = val.parse::<u32>() {
                    version = v;
                }
            }
        }
    }
    if version >= 2 {
        return Err(format!("unknown repository format version: {version}"));
    }
    Ok(())
}

#[cfg(test)]
mod index_write_fail_inject {
    use std::cell::RefCell;

    thread_local! {
        pub static INJECT: RefCell<bool> = const { RefCell::new(false) };
    }
}

/// Debug-only: fail the next [`Repository::write_index`] call.
#[cfg(test)]
pub fn set_test_inject_index_write_fail(enabled: bool) {
    index_write_fail_inject::INJECT.with(|c| *c.borrow_mut() = enabled);
}

#[cfg(test)]
fn test_inject_index_write_fail() -> Result<()> {
    index_write_fail_inject::INJECT.with(|c| {
        if *c.borrow() {
            return Err(Error::Io(std::io::Error::other(
                "injected index write failure",
            )));
        }
        Ok(())
    })
}

#[cfg(not(test))]
fn test_inject_index_write_fail() -> Result<()> {
    Ok(())
}
