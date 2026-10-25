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
//!
//! Library guide: [repo](https://grit-scm.com/docs/library/repository)
//!
//! ```rust
//! use grit_lib::repo::{InitOptions, Repository};
//! # fn main() -> grit_lib::error::Result<()> {
//! let dir = tempfile::tempdir()?;
//! let _repo = Repository::init(dir.path(), &InitOptions::default())?;
//! # Ok(())
//! # }
//! ```

use std::collections::BTreeSet;
use std::fs;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::change_id::ChangeIdGenerator;
use crate::check_ref_format::validate_branch_short_name;
use crate::command_runner::CommandRunner;
use crate::config::{
    config_file_stamps, stamp_for_path, ConfigCacheKey, ConfigFile, ConfigScope, ConfigSet,
    LoadConfigOptions,
};
use crate::diagnostics::{self, DiagnosticsHandle};
use crate::environment::{Environment, RepositoryOptions};
use crate::error::{Error, Result};
use crate::git_path::path_for_error_message;
use crate::hooks::run_hook;
use crate::index::Index;
use crate::init_filesystem::{apply_init_filesystem_config, InitFilesystemConfigOptions};
use crate::objects::{parse_commit, HashAlgo, ObjectId};
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

/// Provenance flags recorded when a repository is discovered or opened explicitly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveryInfo {
    /// Opened via `GIT_DIR` or an explicit git directory (not upward discovery).
    pub explicit_git_dir: bool,
    /// Directory where `.git` / a gitfile was found during upward discovery.
    pub discovery_root: Option<PathBuf>,
    /// `GIT_WORK_TREE` was applied after discovery without `GIT_DIR`.
    pub work_tree_from_env: bool,
    /// Discovery encountered a `.git` gitfile rather than a directory.
    pub discovery_via_gitfile: bool,
}

/// Result of [`Repository::init_with_outcome`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InitOutcome {
    /// `true` when an existing repository was reinitialized in place.
    pub reinitialized: bool,
    /// Branch name recorded in `HEAD` after init (existing branch on reinit).
    pub initial_branch: String,
}

/// Options for [`Repository::init`].
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct InitOptions {
    /// Create a bare repository at `path` instead of `path/.git`.
    pub bare: bool,
    /// Initial branch name written into `HEAD` when creating a new repository.
    ///
    /// When `None`, [`resolve_initial_branch`] reads `init.defaultBranch` from the
    /// configuration cascade (without the new repository's local config) and falls
    /// back to `main`. Ignored on reinit (existing `HEAD` is kept).
    pub initial_branch: Option<String>,
    /// Optional template directory (empty path skips hook installation).
    pub template_dir: Option<PathBuf>,
    /// Ref storage backend for the new repository.
    pub ref_storage: crate::ref_storage::RefStorageFormat,
    /// When set, place the git directory here and link from `path` via a gitfile.
    pub separate_git_dir: Option<PathBuf>,
    /// Bare git directory with `core.worktree` pointing at `path` (`GIT_WORK_TREE` clone layout).
    pub bare_with_worktree_at_path: bool,
    /// Object hash algorithm for a new repository (`extensions.objectformat`).
    ///
    /// When `None`, new repositories default to SHA-1; reinit preserves the existing format.
    pub object_format: Option<HashAlgo>,
    /// Environment used to resolve `init.defaultBranch` (and other init-time config).
    pub environment: Option<Environment>,
}

impl Default for InitOptions {
    fn default() -> Self {
        Self {
            bare: false,
            initial_branch: None,
            template_dir: None,
            ref_storage: crate::ref_storage::RefStorageFormat::Files,
            separate_git_dir: None,
            bare_with_worktree_at_path: false,
            object_format: None,
            environment: None,
        }
    }
}

impl InitOptions {
    /// Same as [`Default::default`].
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Set [`Self::bare`].
    #[must_use]
    pub fn with_bare(mut self, bare: bool) -> Self {
        self.bare = bare;
        self
    }

    /// Set [`Self::initial_branch`].
    #[must_use]
    pub fn with_initial_branch(mut self, initial_branch: impl Into<String>) -> Self {
        self.initial_branch = Some(initial_branch.into());
        self
    }

    /// Set [`Self::object_format`].
    #[must_use]
    pub fn with_object_format(mut self, object_format: HashAlgo) -> Self {
        self.object_format = Some(object_format);
        self
    }

    /// Set [`Self::environment`].
    #[must_use]
    pub fn with_environment(mut self, environment: Environment) -> Self {
        self.environment = Some(environment);
        self
    }

    /// Set [`Self::ref_storage`].
    #[must_use]
    pub fn with_ref_storage(mut self, ref_storage: crate::ref_storage::RefStorageFormat) -> Self {
        self.ref_storage = ref_storage;
        self
    }

    /// Set [`Self::template_dir`].
    #[must_use]
    pub fn with_template_dir(mut self, template_dir: Option<PathBuf>) -> Self {
        self.template_dir = template_dir;
        self
    }

    /// Set [`Self::separate_git_dir`].
    #[must_use]
    pub fn with_separate_git_dir(mut self, separate_git_dir: Option<PathBuf>) -> Self {
        self.separate_git_dir = separate_git_dir;
        self
    }

    /// Set [`Self::bare_with_worktree_at_path`].
    #[must_use]
    pub fn with_bare_with_worktree_at_path(mut self, bare_with_worktree_at_path: bool) -> Self {
        self.bare_with_worktree_at_path = bare_with_worktree_at_path;
        self
    }
}

/// Optional custom ODB (and ref store) wiring for [`RepositoryBuilder::open`].
///
/// # On-disk layout
///
/// What `open` requires in the git directory depends on what is injected:
///
/// - Nothing injected: a normal git directory (`HEAD`, `objects/`).
/// - A [`Self::ref_store`]: no `HEAD` file (the store holds `HEAD`), and `refs/` is never read.
/// - A custom [`OdbBuilder::primary`](crate::odb::OdbBuilder::primary): no `objects/`.
/// - Both: nothing at all. The git directory may be empty or missing; it is still where the
///   repository config (`config`) is read from and where the index lives once something
///   stages files, so create it before using index-based operations.
///
/// ```
/// use std::sync::Arc;
///
/// use grit_lib::config::environment::RepositoryOptions;
/// use grit_lib::objects::odb::store::MemoryStore;
/// use grit_lib::objects::odb::OdbBuilder;
/// use grit_lib::objects::HashAlgo;
/// use grit_lib::refs::store::{MemoryRefStore, RawRef};
/// use grit_lib::repo::RepositoryBuilder;
///
/// # fn main() -> grit_lib::error::Result<()> {
/// let dir = tempfile::tempdir()?;
/// let git_dir = dir.path().join("in-memory.git");
/// let repo = RepositoryBuilder::new(RepositoryOptions::empty())
///     .odb_builder(
///         OdbBuilder::files(git_dir.join("objects"))
///             .primary(Arc::new(MemoryStore::new(HashAlgo::Sha1)))
///             .alternates(false),
///     )
///     .ref_store(Arc::new(MemoryRefStore::new()))
///     .initial_branch("main")
///     .open(&git_dir, None)?;
/// assert_eq!(
///     repo.refs().read_raw("HEAD")?,
///     Some(RawRef::Symbolic("refs/heads/main".to_owned()))
/// );
/// assert!(!git_dir.exists());
/// # Ok(())
/// # }
/// ```
#[must_use]
pub struct RepositoryBuilder {
    options: RepositoryOptions,
    odb_builder: Option<crate::odb::OdbBuilder>,
    ref_store: Option<Arc<dyn crate::refs::store::RefStore>>,
    initial_branch: Option<String>,
}

impl RepositoryBuilder {
    /// Start building a repository open with `options`.
    pub fn new(options: RepositoryOptions) -> Self {
        Self {
            options,
            odb_builder: None,
            ref_store: None,
            initial_branch: None,
        }
    }

    /// Use a preconfigured [`crate::odb::OdbBuilder`] for the object store.
    ///
    /// The builder's objects directory may be any path naming the repository's `objects/`
    /// directory, or [`crate::environment::Environment::git_object_directory`] when that is
    /// set (through symlinks, or relative to the environment's current directory);
    /// [`Self::open`] replaces it with the canonical path of that directory, which is what
    /// [`crate::odb::Odb::objects_dir`] then reports (the symlink target when `objects/`
    /// itself is a symlink).
    pub fn odb_builder(mut self, builder: crate::odb::OdbBuilder) -> Self {
        self.odb_builder = Some(builder);
        self
    }

    /// Use `store` as the repository's ref backend (for example
    /// [`crate::refs::store::MemoryRefStore`]) instead of the one the git directory selects.
    ///
    /// The default backend is never opened, so the git directory needs no `HEAD` or `refs/`.
    pub fn ref_store(mut self, store: Arc<dyn crate::refs::store::RefStore>) -> Self {
        self.ref_store = Some(store);
        self
    }

    /// When the ref store has no `HEAD` after opening, point it at `refs/heads/<branch>`
    /// (an unborn branch), as [`Repository::init`] does for a new git directory.
    ///
    /// An existing `HEAD` is left alone. Mostly useful with a fresh [`Self::ref_store`].
    pub fn initial_branch(mut self, branch: impl Into<String>) -> Self {
        self.initial_branch = Some(branch.into());
        self
    }

    /// Open `git_dir` with optional `work_tree`, applying builder overrides.
    ///
    /// With an [`Self::odb_builder`], relative `git_dir`, `work_tree` and objects paths are
    /// resolved against the environment's current directory
    /// ([`crate::environment::Environment::discovery_cwd`]), and paths through symlinks are
    /// accepted.
    ///
    /// # Errors
    ///
    /// Same as [`Repository::open`], plus:
    /// - [`Error::OdbObjectsDirMismatch`] when a files-backed [`Self::odb_builder`] names a
    ///   directory other than the repository's `objects/` (or the environment's
    ///   `GIT_OBJECT_DIRECTORY`).
    /// - [`Error::InvalidRef`] when [`Self::initial_branch`] is not a valid branch name.
    /// - [`Error::RefStore`] when seeding `HEAD` in the ref store fails.
    pub fn open(self, git_dir: &Path, work_tree: Option<&Path>) -> Result<Repository> {
        let mut options = self.options;
        if let Some(wt) = work_tree {
            options.work_tree = Some(wt.to_path_buf());
        }
        let repo = if let Some(builder) = self.odb_builder {
            Repository::open_with_odb_internal(&options, git_dir, builder, self.ref_store)?
        } else {
            Repository::open_internal_with_refs(&options, git_dir, self.ref_store)?
        };
        if let Some(branch) = self.initial_branch {
            seed_unborn_head(repo.refs(), &branch)?;
        }
        Ok(repo)
    }
}

/// Point `HEAD` at `refs/heads/<branch>` in `store` unless `HEAD` already exists.
fn seed_unborn_head(store: &dyn crate::refs::store::RefStore, branch: &str) -> Result<()> {
    use crate::refs::store::{Expected, RawRef, RefStoreError, RefTransaction, RefUpdate};

    validate_branch_short_name(branch).map_err(|e| Error::InvalidRef(e.to_string()))?;
    if store.read_raw("HEAD")?.is_some() {
        return Ok(());
    }
    let txn = RefTransaction::new().update(RefUpdate {
        name: "HEAD".to_owned(),
        new_value: Some(RawRef::Symbolic(format!("refs/heads/{branch}"))),
        expected: Expected::Missing,
        reflog: None,
        flags: crate::refs::store::RefUpdateFlags {
            no_deref: true,
            ..Default::default()
        },
    })?;
    match store.prepare(txn).and_then(|prepared| prepared.commit()) {
        Ok(()) | Err(RefStoreError::ExpectedMismatch { .. }) => Ok(()),
        Err(err) => Err(err.into()),
    }
}

/// A handle to an open Git repository.
///
/// ## Config and path caching
///
/// On open, Grit resolves the common git directory ([`Self::common_git_dir`]), loads the merged
/// configuration cascade once, and keeps that snapshot for the lifetime of this handle. Hot paths
/// (object reads, replace-ref checks, and similar) reuse the snapshot without re-statting config
/// files on every access.
///
/// Refresh the snapshot when on-disk config may have changed:
///
/// - [`Self::reload_config`] after writing config through [`crate::config::ConfigHandle`] or other
///   in-process library APIs on this handle.
/// - Hook and filter subprocesses may rewrite config; [`crate::hooks::run_hook_opts`] reloads the
///   snapshot after hooks finish when a [`Repository`] is provided.
/// - External edits (another process, or the system `git` binary) are picked up on the next
///   [`Self::config`] call, which revalidates file stamps once per call—not on every internal read.
/// - Open a new [`Repository`] to observe ref or config changes made elsewhere without calling
///   [`Self::reload_config`] (for example a `refs/replace/*` ref created by system `git`).
///
/// # Examples
///
/// ```
/// use grit_lib::prelude::RepositoryOptions;
/// use grit_lib::repo::{InitOptions, Repository};
///
/// # fn main() -> grit_lib::error::Result<()> {
/// let dir = tempfile::tempdir()?;
/// let repo = Repository::init(dir.path(), &InitOptions::default())?;
/// let reopened = Repository::open(
///     repo.git_dir(),
///     &RepositoryOptions::empty().with_work_tree(dir.path()),
/// )?;
/// assert_eq!(reopened.git_dir(), repo.git_dir());
/// # Ok(())
/// # }
/// ```
pub struct Repository {
    git_dir: PathBuf,
    /// Common git directory (`commondir` target when linked; otherwise [`Self::git_dir`]).
    common_git_dir: PathBuf,
    work_tree: Option<PathBuf>,
    odb: Odb,
    explicit_git_dir: bool,
    discovery_root: Option<PathBuf>,
    work_tree_from_env: bool,
    discovery_via_gitfile: bool,
    /// Session config snapshot and derived hot-path flags (see struct-level rustdoc).
    config_session: Mutex<Option<ConfigSession>>,
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
    /// Injectable random bytes for new change ids.
    change_id_generator: Arc<dyn ChangeIdGenerator>,
    diagnostics: DiagnosticsHandle,
    network_trace: bool,
    /// Embedder override for rev-parse relative dates (`@{yesterday}`, etc.).
    reference_unix_time: Option<i64>,
    test_assume_different_owner: bool,
    force_split_index: bool,
    thread_policy: crate::parallel::ThreadPolicy,
    hook_stdio: crate::hooks::HookStdio,
}

const _: () = {
    fn assert_send_sync<T: Send + Sync>() {}
    let _ = assert_send_sync::<Repository>;
};

impl std::fmt::Debug for Repository {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Repository")
            .field("git_dir", &self.git_dir())
            .field("work_tree", &self.work_tree())
            .field("explicit_git_dir", &self.discovery_info().explicit_git_dir)
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

/// Stamp metadata for one loaded config cascade (session invalidation).
#[derive(Clone)]
struct ConfigSessionValidation {
    key: ConfigCacheKey,
    env_fingerprint: Vec<(String, Option<String>)>,
    base_stamps: Vec<(PathBuf, Option<(std::time::SystemTime, u64)>)>,
    extra_stamps: Vec<(PathBuf, Option<(std::time::SystemTime, u64)>)>,
}

/// In-memory config session for one open [`Repository`] handle.
struct ConfigSession {
    config: Arc<ConfigSet>,
    validation: ConfigSessionValidation,
    cached_settings: RepoCachedSettings,
    /// When `true`, [`Repository::resolve_replace_redirect`] skips replace-ref lookups.
    replace_lookup_inactive: bool,
}

impl ConfigSessionValidation {
    fn still_valid(&self, env: &Environment, git_dir: &Path, opts: &LoadConfigOptions) -> bool {
        let Some(env_fp) = env.config_fingerprint() else {
            return false;
        };
        if self.env_fingerprint.as_slice() != env_fp.as_slice() {
            return false;
        }
        let key = ConfigCacheKey::new(Some(git_dir), opts);
        if key != self.key {
            return false;
        }
        let base_stamps = config_file_stamps(env, Some(git_dir), opts);
        if self.base_stamps.as_slice() != base_stamps.as_slice() {
            return false;
        }
        for (path, stamp) in &self.extra_stamps {
            if stamp_for_path(path) != *stamp {
                return false;
            }
        }
        true
    }
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
            repo_caches: Some(Arc::clone(&self.caches)),
            ..Default::default()
        }
    }

    fn from_canonical_git_dir(
        options: &RepositoryOptions,
        git_dir: PathBuf,
        work_tree: Option<&Path>,
        injected_refs: Option<Arc<dyn crate::refs::store::RefStore>>,
    ) -> Result<Self> {
        let environment = Arc::new(options.environment.clone());
        let command_runner = Arc::clone(&options.command_runner);
        let change_id_generator = Arc::clone(&options.change_id_generator);
        // An injected ref store holds HEAD itself; otherwise HEAD must exist or be a symlink
        // (linked worktrees have a symlink HEAD).
        if injected_refs.is_none() && !head_file_present(&git_dir) {
            return Err(not_a_repository_at(&git_dir));
        }

        let common_git_dir = resolve_common_dir(&git_dir).unwrap_or_else(|| git_dir.clone());
        let objects_dir = resolve_objects_dir(&environment, &git_dir, &common_git_dir);

        if !objects_dir.exists() {
            return Err(not_a_repository_at(&git_dir));
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
        caches.set_promisor_fetcher(options.promisor_fetcher.clone());
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

        let ref_store = open_or_install_ref_store(&caches, &git_dir, &environment, injected_refs)?;

        Ok(Self {
            git_dir,
            common_git_dir,
            work_tree,
            odb,
            explicit_git_dir: false,
            discovery_root: None,
            work_tree_from_env: false,
            discovery_via_gitfile: false,
            config_session: Mutex::new(None),
            caches,
            ref_store,
            environment,
            git_prefix,
            command_runner,
            change_id_generator,
            diagnostics: Arc::clone(&options.diagnostics),
            network_trace: options.network_trace,
            reference_unix_time: options.reference_unix_time,
            test_assume_different_owner: options.test_assume_different_owner,
            force_split_index: options.force_split_index,
            thread_policy: options.thread_policy,
            hook_stdio: options.hook_stdio,
        })
    }

    /// Explicit parallel worker overrides for this handle (`--threads` / embedder policy).
    #[must_use]
    pub fn thread_policy(&self) -> crate::parallel::ThreadPolicy {
        self.thread_policy
    }

    /// Return a copy of this repository handle with an updated parallel thread policy.
    #[must_use]
    pub fn with_thread_policy(mut self, policy: crate::parallel::ThreadPolicy) -> Self {
        self.thread_policy = policy;
        self
    }

    /// Return this handle's shared cache arena.
    #[must_use]
    pub(crate) fn caches(&self) -> &Arc<RepoCaches> {
        &self.caches
    }

    /// Return the promisor hydration hook for this repository, if installed.
    #[must_use]
    pub(crate) fn promisor_hydrate_hook(&self) -> Option<crate::blame::PromisorHydrateHook> {
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

    /// [`Self::wall_clock_epoch`] in this handle's `TZ`, for stamping author, committer and
    /// reflog identity lines (see
    /// [`resolve_identity_with`](crate::config::ident_resolve::resolve_identity_with)).
    #[must_use]
    pub fn identity_now(&self) -> time::OffsetDateTime {
        crate::environment::offset_from_epoch_and_tz(
            self.wall_clock_epoch(),
            self.environment.tz.as_deref(),
        )
    }

    /// Resolve the author or committer identity Git would record for a new commit, tag, stash,
    /// or notes commit made through this handle.
    ///
    /// Reads the handle's [`Environment`] (`GIT_AUTHOR_*` / `GIT_COMMITTER_*`, including the
    /// `*_DATE` overrides) and configuration (`author.*`, `committer.*`, `user.*`); see
    /// [`resolve_signature_with`](crate::config::ident_resolve::resolve_signature_with).
    ///
    /// # Parameters
    ///
    /// - `role`: author or committer.
    /// - `now`: timestamp recorded when no date override is set; usually [`Self::identity_now`].
    ///
    /// # Errors
    ///
    /// [`Error::Identity`] when no usable name, email, or date is configured, and config load
    /// failures.
    ///
    /// # Examples
    ///
    /// ```
    /// use grit_lib::config::ident_resolve::IdentRole;
    /// use grit_lib::repo::{InitOptions, Repository};
    /// # fn main() -> grit_lib::error::Result<()> {
    /// let dir = tempfile::tempdir()?;
    /// let repo = Repository::init(dir.path(), &InitOptions::default())?;
    /// match repo.identity(IdentRole::Author, repo.identity_now()) {
    ///     Ok(author) => assert!(!author.email.is_empty()),
    ///     Err(grit_lib::error::Error::Identity(_)) => {} // nothing configured
    ///     Err(other) => return Err(other),
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub fn identity(
        &self,
        role: crate::config::ident_resolve::IdentRole,
        now: time::OffsetDateTime,
    ) -> Result<crate::ident::Signature> {
        let config = self.config()?;
        Ok(crate::config::ident_resolve::resolve_signature_with(
            self.environment(),
            &config,
            role,
            now,
        )?)
    }

    /// Subprocess runner used for hooks, filters, and helpers.
    #[must_use]
    pub fn command_runner(&self) -> Arc<dyn CommandRunner> {
        Arc::clone(&self.command_runner)
    }

    /// Where hooks run through this handle write their output
    /// ([`RepositoryOptions::with_hook_stdio`]).
    #[must_use]
    pub fn hook_stdio(&self) -> crate::hooks::HookStdio {
        self.hook_stdio
    }

    /// Generator used for random change ids on new commits.
    #[must_use]
    pub fn change_id_generator(&self) -> Arc<dyn ChangeIdGenerator> {
        Arc::clone(&self.change_id_generator)
    }

    /// [`RepositoryOptions`] for discovering a linked worktree of this repository.
    ///
    /// Inherits config discovery ([`Environment`], command runner, diagnostics, thread policy)
    /// from the caller handle but clears worktree-specific discovery overrides so
    /// [`Repository::discover`] resolves the linked tree's own `.git` gitfile and index.
    #[must_use]
    pub fn linked_worktree_discover_options(&self) -> RepositoryOptions {
        let mut environment = (*self.environment).clone();
        environment.git_dir = None;
        environment.git_work_tree = None;
        environment.git_index_file = None;
        environment.git_prefix = None;
        environment.git_object_directory = None;
        RepositoryOptions {
            environment,
            command_runner: Arc::clone(&self.command_runner),
            change_id_generator: Arc::clone(&self.change_id_generator),
            diagnostics: Arc::clone(&self.diagnostics),
            network_trace: self.network_trace,
            test_assume_different_owner: self.test_assume_different_owner,
            force_split_index: self.force_split_index,
            reference_unix_time: self.reference_unix_time,
            thread_policy: self.thread_policy,
            work_tree: None,
            validate_repository_format: true,
            open_for_serving: false,
            promisor_fetcher: self.caches().promisor_fetcher(),
            hook_stdio: self.hook_stdio,
        }
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
        self.config_arc_with_validation(true)
    }

    /// Hot-path config access: session snapshot without stamp revalidation.
    pub(crate) fn ensure_config_arc_fast(&self) -> Result<Arc<ConfigSet>> {
        self.config_arc_with_validation(false)
    }

    fn config_arc_with_validation(&self, validate_stamps: bool) -> Result<Arc<ConfigSet>> {
        if validate_stamps {
            if let Ok(guard) = self.config_session.lock() {
                if let Some(session) = guard.as_ref() {
                    let opts = self.config_load_options(true);
                    if session.validation.still_valid(
                        self.environment.as_ref(),
                        &self.git_dir,
                        &opts,
                    ) {
                        return Ok(Arc::clone(&session.config));
                    }
                }
            }
        } else if let Ok(guard) = self.config_session.lock() {
            if let Some(session) = guard.as_ref() {
                return Ok(Arc::clone(&session.config));
            }
        }
        self.refresh_config_session()?;
        self.config_session
            .lock()
            .map_err(|e| Error::Message(format!("config session lock poisoned: {e}")))?
            .as_ref()
            .map(|s| Arc::clone(&s.config))
            .ok_or_else(|| Error::Message("config session missing after refresh".to_owned()))
    }

    fn refresh_config_session(&self) -> Result<()> {
        crate::trace::trace_span!("grit.config.load");
        let opts = self.config_load_options(true);
        if let Ok(guard) = self.config_session.lock() {
            if let Some(session) = guard.as_ref() {
                if session
                    .validation
                    .still_valid(self.environment.as_ref(), &self.git_dir, &opts)
                {
                    return Ok(());
                }
                let env = self.environment.as_ref();
                let base_stamps = config_file_stamps(env, Some(&self.git_dir), &opts);
                if session.validation.base_stamps.as_slice() != base_stamps.as_slice() {
                    self.caches.invalidate_config_file_derived();
                }
            }
        }
        self.caches.clear_session_config();
        let env = self.environment.as_ref();
        let cfg = self
            .caches
            .load_config_with_options(env, Some(&self.git_dir), &opts)?;
        let key = ConfigCacheKey::new(Some(&self.git_dir), &opts);
        let (env_fingerprint, base_stamps, extra_stamps) =
            self.caches.config_entry_stamps(&key).unwrap_or_else(|| {
                (
                    env.config_fingerprint().unwrap_or_default(),
                    config_file_stamps(env, Some(&self.git_dir), &opts),
                    Vec::new(),
                )
            });
        let cached_settings =
            Self::repo_cached_settings_from_config(cfg.as_ref(), self.environment.as_ref());
        let replace_lookup_inactive = self.replace_lookup_inactive(cfg.as_ref());
        self.caches
            .set_session_config(key.clone(), Arc::clone(&cfg));
        let session = ConfigSession {
            config: Arc::clone(&cfg),
            validation: ConfigSessionValidation {
                key,
                env_fingerprint,
                base_stamps,
                extra_stamps,
            },
            cached_settings,
            replace_lookup_inactive,
        };
        *self
            .config_session
            .lock()
            .map_err(|e| Error::Message(format!("config session lock poisoned: {e}")))? =
            Some(session);
        Ok(())
    }

    fn replace_lookup_inactive(&self, cfg: &ConfigSet) -> bool {
        if self.environment.git_no_replace_objects {
            return true;
        }
        if !cfg
            .get_bool("core.useReplaceRefs")
            .and_then(|r| r.ok())
            .unwrap_or(true)
        {
            return true;
        }
        let replace_dir = self.common_git_dir.join("refs/replace");
        !replace_dir_is_non_empty(&replace_dir)
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
        if let Ok(mut guard) = self.config_session.lock() {
            *guard = None;
        }
        self.refresh_config_session()?;
        Ok(())
    }

    /// Warm repository caches after discovery (reftable backend flag, ref store).
    pub(crate) fn install_config_snapshot(&self) {
        let _ = self
            .caches
            .open_ref_store(&self.git_dir, self.environment.as_ref());
    }

    /// Common git directory for this handle (`commondir` when linked).
    #[must_use]
    pub fn common_git_dir(&self) -> &Path {
        &self.common_git_dir
    }

    fn validate_repository_format_cached(&self) -> Result<()> {
        let Some(config_path) = repository_config_path(&self.git_dir) else {
            return Ok(());
        };
        let parsed = self.caches.repository_format(&self.git_dir, &config_path)?;
        validate_repository_format_parsed(&parsed)
    }

    /// Reference storage backend for this repository.
    #[must_use]
    pub fn refs(&self) -> &dyn crate::refs::store::RefStore {
        self.ref_store.as_ref()
    }

    /// Whether [`Self::refs`] is an on-disk files or reftable backend, so `HEAD` and refs can
    /// also be read from the git directory. False for injected stores such as
    /// [`crate::refs::store::MemoryRefStore`].
    #[must_use]
    pub(crate) fn refs_on_disk(&self) -> bool {
        use crate::refs::store::RefStorageFormat;
        matches!(
            self.ref_store.format(),
            RefStorageFormat::Files | RefStorageFormat::Reftable
        )
    }

    /// Object read/write handle for this repository.
    #[inline]
    #[must_use]
    pub fn objects(&self) -> crate::objects::Objects<'_> {
        crate::objects::Objects::new(self)
    }

    /// Reference resolve/list/update and reflog read handle.
    #[inline]
    #[must_use]
    pub fn references(&self) -> crate::refs::References<'_> {
        crate::refs::References::new(self)
    }

    /// Index load/write handle.
    #[inline]
    #[must_use]
    pub fn index_handle(&self) -> crate::index::IndexHandle<'_> {
        crate::index::IndexHandle::new(self)
    }

    /// Merged config read and local config write handle.
    #[inline]
    #[must_use]
    pub fn config_handle(&self) -> crate::config::ConfigHandle<'_> {
        crate::config::ConfigHandle::new(self)
    }

    /// Revision parse and rev-list handle.
    #[inline]
    #[must_use]
    pub fn rev_walk(&self) -> crate::revwalk::RevWalk<'_> {
        crate::revwalk::RevWalk::new(self)
    }

    /// Tree, index, and worktree diff handle.
    #[inline]
    #[must_use]
    pub fn diff(&self) -> crate::diff::Diff<'_> {
        crate::diff::Diff::new(self)
    }

    /// Remote configuration, `ls-remote`, fetch, and push for this repository.
    #[must_use]
    pub fn remotes(&self) -> crate::repo_handles::RepositoryRemotes<'_> {
        crate::repo_handles::RepositoryRemotes::new(self)
    }

    /// Linked worktree listing and admin for this repository.
    #[must_use]
    pub fn worktrees(&self) -> crate::repo_handles::RepositoryWorktrees<'_> {
        crate::repo_handles::RepositoryWorktrees::new(self)
    }

    /// Stash stack operations for this repository.
    #[must_use]
    pub fn stash(&self) -> crate::repo_handles::RepositoryStash<'_> {
        crate::repo_handles::RepositoryStash::new(self)
    }

    /// Git notes read/write for this repository.
    #[must_use]
    pub fn notes(&self) -> crate::repo_handles::RepositoryNotes<'_> {
        crate::repo_handles::RepositoryNotes::new(self)
    }

    /// Hook script discovery for this repository.
    #[must_use]
    pub fn hooks(&self) -> crate::repo_handles::RepositoryHooks<'_> {
        crate::repo_handles::RepositoryHooks::new(self)
    }

    /// Check object store connectivity, refs, and optional pack indexes.
    ///
    /// Progress is reported through `progress`; the library never prints.
    ///
    /// # Errors
    ///
    /// Propagates object-store, ref, or index failures that abort the run. A custom primary
    /// object store is validated through the objects it lists (see [`crate::fsck::fsck`]).
    pub fn fsck(
        &self,
        options: &crate::fsck::FsckOptions,
        progress: &mut dyn crate::progress::ProgressSink,
        cancel: &dyn crate::progress::Cancel,
    ) -> crate::error::Result<crate::fsck::FsckReport> {
        crate::fsck::fsck(self, options, progress, cancel)
    }

    /// SHA-256 content digest of a revision's tree ([tree-sha256] spec).
    ///
    /// [tree-sha256]: https://github.com/schacon/tree-sha256
    ///
    /// # Errors
    ///
    /// Returns [`crate::error::Error::TreeSha256MissingSubmodules`] when submodule
    /// checkouts or objects needed for hashing are absent.
    pub fn tree_sha256(
        &self,
        oid: crate::objects::ObjectId,
        options: &crate::tree_sha256::TreeSha256Options,
    ) -> crate::error::Result<crate::tree_sha256::TreeSha256Report> {
        crate::tree_sha256::tree_sha256(self, oid, options)
    }

    /// Verify an annotated tag's signature and optional `tree-sha256` header.
    ///
    /// Reads the tag object by `oid` (not via a ref). Signature checking and digest
    /// recomputation run on separate threads when both are needed.
    ///
    /// # Errors
    ///
    /// See [`crate::tree_sha256::verify_tag`].
    pub fn verify_tag(
        &self,
        oid: crate::objects::ObjectId,
        options: &crate::tree_sha256::TagVerifyOptions,
    ) -> crate::error::Result<crate::tree_sha256::TagVerification> {
        crate::tree_sha256::verify_tag(self, oid, options)
    }

    /// Patch-ID for a single commit (merge commits and empty diffs return `None`).
    ///
    /// # Errors
    ///
    /// Propagates object database read/parse failures.
    pub fn patch_id(
        &self,
        commit: &crate::objects::ObjectId,
        mode: crate::patch_ids::PatchIdMode,
    ) -> crate::error::Result<Option<crate::objects::ObjectId>> {
        crate::patch_ids::compute_patch_id(self.odb().hash_algo(), self.odb(), commit, mode)
    }

    /// Reflog listing, deletion, and expiry for this repository.
    #[must_use]
    pub fn reflogs(&self) -> crate::repo_handles::RepositoryReflogs<'_> {
        crate::repo_handles::RepositoryReflogs::new(self)
    }

    /// Bundle create, verify, list-heads, and unbundle for this repository.
    #[must_use]
    pub fn bundles(&self) -> crate::repo_handles::RepositoryBundles<'_> {
        crate::repo_handles::RepositoryBundles::new(self)
    }

    /// Repack reachable objects into a single pack and prune redundant storage.
    ///
    /// # Errors
    ///
    /// Propagates pack, prune, or unsupported-backend failures from [`crate::maintenance::repack`].
    pub fn repack(
        &self,
        opts: &crate::maintenance::RepackOptions,
    ) -> crate::error::Result<crate::maintenance::RepackReport> {
        crate::maintenance::repack(self, opts)
    }

    /// Resolve `refname` through this repository's ref store.
    ///
    /// # Errors
    ///
    /// Propagates [`crate::refs::store::RefStoreError`] as [`Error::RefStore`].
    pub(crate) fn resolve_ref_oid(&self, refname: &str) -> Result<crate::objects::ObjectId> {
        self.ref_store.resolve(refname)
    }

    /// Replace the ref store (embedders injecting [`crate::refs::store::MemoryRefStore`]).
    #[must_use]
    pub fn with_ref_store(mut self, store: Arc<dyn crate::refs::store::RefStore>) -> Self {
        self.caches
            .install_ref_store(&self.git_dir, Arc::clone(&store));
        self.ref_store = store;
        self
    }

    /// Whether pathspec matching should NFC-normalize paths for this repository.
    #[must_use]
    pub(crate) fn pathspec_precompose_enabled(&self) -> bool {
        self.caches
            .pathspec_precompose_enabled(self.environment.as_ref(), &self.git_dir)
    }

    /// Pretend the work tree aliases NFD/NFC spellings, as APFS and HFS+ do, so unit tests on
    /// other filesystems exercise the precomposed pathspec matching macOS users get.
    #[cfg(test)]
    pub(crate) fn force_pathspec_precompose_for_test(&self) {
        self.caches.force_pathspec_precompose_for_test();
    }

    /// Resolve one argv pathspec against this repository's work tree.
    ///
    /// `prefix` is the work-tree-relative directory the pathspec was typed in (`None` at the
    /// root). Magic pathspecs pass through; plain paths are joined with `prefix`, normalized, and
    /// NFC-normalized when `core.precomposeunicode` is on and the filesystem aliases NFD/NFC
    /// spellings. That decision is computed once per handle, so resolving many pathspecs costs no
    /// config reload or filesystem probe per path.
    ///
    /// # Errors
    ///
    /// [`crate::pathspec::PathspecResolveError::NeedsWorkTree`] for a bare repository, and
    /// [`crate::pathspec::PathspecResolveError::OutsideRepository`] when `pathspec` resolves
    /// outside the work tree.
    pub fn resolve_worktree_pathspec(
        &self,
        pathspec: &str,
        prefix: Option<&str>,
    ) -> std::result::Result<String, crate::pathspec::PathspecResolveError> {
        let work_tree = self
            .work_tree()
            .ok_or(crate::pathspec::PathspecResolveError::NeedsWorkTree)?;
        Ok(
            crate::pathspec::resolve_pathspec_in_worktree_with_argv_precompose(
                pathspec,
                pathspec,
                work_tree,
                prefix,
                self.pathspec_precompose_enabled(),
            )?,
        )
    }

    #[must_use]
    pub(crate) fn pathspec_match_context(&self) -> crate::pathspec::PathspecMatchContext {
        crate::pathspec::pathspec_match_context_with_precompose(self.pathspec_precompose_enabled())
    }

    /// Whether `path` is selected by the resolved pathspec list `specs`.
    ///
    /// Shorthand for [`Self::pathspec_matcher`] followed by
    /// [`crate::pathspec::PathspecMatcher::matches`]; build the matcher once instead when
    /// filtering many paths with specs that may carry `:(attr:...)` magic.
    ///
    /// # Errors
    ///
    /// Same as [`Self::pathspec_matcher`].
    pub fn paths_match_pathspecs(&self, path: &str, specs: &[String]) -> Result<bool> {
        Ok(self.pathspec_matcher(specs)?.matches(path))
    }

    /// Prepare `specs` (resolved pathspecs) for matching many paths in this repository.
    ///
    /// The matcher applies Git's list semantics and every pathspec magic word, including
    /// `:(attr:...)`, which is evaluated against this repository's attributes: the work-tree
    /// stack (global file, root and nested `.gitattributes`, `info/attributes`), or
    /// `info/attributes` and the global file for a bare repository. Attributes are read only
    /// when a spec carries attr magic, once per matcher, from the per-handle cache; `[attr]`
    /// macros are expanded and `core.ignorecase` makes attribute patterns case-insensitive.
    /// Paths and specs follow `core.precomposeunicode`.
    ///
    /// # Errors
    ///
    /// Only when a spec carries attr magic: [`Error::InvalidAttrPathspec`] for a malformed or
    /// repeated `attr:` clause, or the error from loading config or the attribute files (for
    /// example an unreadable `core.attributesFile` setting or a poisoned cache lock).
    pub fn pathspec_matcher<'s>(
        &self,
        specs: &'s [String],
    ) -> Result<crate::pathspec::PathspecMatcher<'s>> {
        let precompose = self.pathspec_precompose_enabled();
        if !specs
            .iter()
            .any(|spec| crate::pathspec::pathspec_has_attr_magic(spec))
        {
            return Ok(crate::pathspec::PathspecMatcher::new(
                specs, precompose, None, false,
            ));
        }
        crate::pathspec::validate_attr_pathspecs(specs)?;
        let attrs = match self.work_tree() {
            Some(work_tree) => self.caches().gitattributes_stack_shared(self, work_tree)?,
            None => self.caches().gitattributes_bare_shared(self)?,
        };
        let icase = crate::crlf::config_ignorecase(self.config()?.as_ref());
        Ok(crate::pathspec::PathspecMatcher::new(
            specs,
            precompose,
            Some(attrs),
            icase,
        ))
    }

    /// Repository-relative prefix of the process cwd for argv pathspecs (no trailing slash).
    ///
    /// Matches Git `GIT_PREFIX` / `pathdiff` semantics used by `git log --`, `git rev-list --`,
    /// and `git blame` from a subdirectory.
    #[must_use]
    pub fn pathspec_cwd_prefix(&self) -> Option<&str> {
        self.git_prefix.as_deref().filter(|p| !p.is_empty())
    }

    /// Cwd prefix for one argv pathspec (`None` for `:(top)` / `:/`, which stay repo-root-relative).
    pub fn pathspec_argv_prefix<'a>(&'a self, pathspec: &str) -> Option<&'a str> {
        if crate::pathspec::pathspec_has_top(pathspec) {
            None
        } else {
            self.pathspec_cwd_prefix()
        }
    }

    /// Resolve argv path operands for revision walks (`log`, `revs`, `rev-list`, …).
    ///
    /// Literal segments are interpreted relative to the process cwd under the work tree, like
    /// `git diff --` and Git pathspec cwd rules. Exclude-only lists gain Git's implicit cwd
    /// positive prefix when appropriate.
    ///
    /// # Errors
    ///
    /// Returns [`Error::PathError`] when a pathspec resolves outside the work tree.
    pub fn resolve_rev_walk_pathspecs(&self, paths: &[String]) -> Result<Vec<String>> {
        if paths.is_empty() {
            return Ok(Vec::new());
        }
        let prefix = self.pathspec_cwd_prefix();
        let cwd_for_implicit = prefix.map(str::to_owned);
        let mut resolved = Vec::with_capacity(paths.len());
        if self.work_tree().is_some() {
            for pathspec in paths {
                let prefix = Self::pathspec_argv_prefix(self, pathspec);
                let spec = self
                    .resolve_worktree_pathspec(pathspec, prefix)
                    .map_err(|e| Error::PathError(e.to_string()))?;
                resolved.push(spec);
            }
        } else {
            let root = self.git_dir();
            for pathspec in paths {
                let prefix = Self::pathspec_argv_prefix(self, pathspec);
                resolved.push(crate::pathspec::resolve_pathspec(pathspec, root, prefix));
            }
        }
        Ok(crate::pathspec::extend_pathspec_list_implicit_cwd(
            &resolved,
            cwd_for_implicit.as_deref(),
        ))
    }

    /// Absolute path to the git directory (`.git/` or bare repository root).
    #[must_use]
    pub fn git_dir(&self) -> &Path {
        &self.git_dir
    }

    /// Absolute work tree path for non-bare repositories.
    #[must_use]
    pub fn work_tree(&self) -> Option<&Path> {
        self.work_tree.as_deref()
    }

    /// Object database for this repository.
    #[must_use]
    pub fn odb(&self) -> &Odb {
        &self.odb
    }

    /// Fetch missing promisor objects in one batch before checkout or other bulk reads.
    ///
    /// # Errors
    ///
    /// Returns [`crate::error::Error::Message`] when no fetcher is installed, or transport failures.
    pub fn prefetch_promised(&self, oids: &[ObjectId]) -> Result<()> {
        let Some(fetcher) = self.caches().promisor_fetcher() else {
            return Err(Error::Message(
                "promisor prefetch requires a PromisorFetcher on RepositoryOptions".into(),
            ));
        };
        fetcher.fetch_missing(self, oids)
    }

    /// Discovery provenance for safe-directory and setup tracing.
    #[must_use]
    pub fn discovery_info(&self) -> DiscoveryInfo {
        DiscoveryInfo {
            explicit_git_dir: self.explicit_git_dir,
            discovery_root: self.discovery_root.clone(),
            work_tree_from_env: self.work_tree_from_env,
            discovery_via_gitfile: self.discovery_via_gitfile,
        }
    }

    fn cached_settings(&self) -> RepoCachedSettings {
        if let Ok(guard) = self.config_session.lock() {
            if let Some(session) = guard.as_ref() {
                return session.cached_settings.clone();
            }
        }
        self.ensure_config_arc_fast()
            .map(|cfg| {
                Self::repo_cached_settings_from_config(cfg.as_ref(), self.environment.as_ref())
            })
            .unwrap_or_else(|_| {
                Self::repo_cached_settings_from_config(&ConfigSet::new(), self.environment.as_ref())
            })
    }

    /// Initialise a repository at `path` and return an open handle.
    ///
    /// # Errors
    ///
    /// Propagates filesystem and layout errors from the underlying init helpers.
    pub fn init(path: &Path, options: &InitOptions) -> Result<Self> {
        Self::init_with_outcome(path, options).map(|(repo, _)| repo)
    }

    /// Like [`Self::init`], but also reports whether the repository was reinitialized and
    /// the effective initial branch name.
    ///
    /// # Errors
    ///
    /// Propagates filesystem and layout errors from the underlying init helpers.
    pub fn init_with_outcome(path: &Path, options: &InitOptions) -> Result<(Self, InitOutcome)> {
        if options.bare && options.separate_git_dir.is_some() {
            return Err(Error::InitBareWithSeparateGitDir);
        }
        let env = options
            .environment
            .clone()
            .unwrap_or_else(Environment::empty);
        let git_dir = resolve_git_dir_for_init(path, options)?;
        let reinitialized = git_dir.join("config").is_file() && git_dir.join("HEAD").is_file();
        let existing_format = hash_algo_from_git_dir(&git_dir)?;
        let object_format = match (reinitialized, options.object_format) {
            (true, None) => existing_format,
            (true, Some(requested)) if requested != existing_format => {
                return Err(Error::InitObjectFormatChange);
            }
            (true, Some(requested)) => requested,
            (false, None) => HashAlgo::Sha1,
            (false, Some(requested)) => requested,
        };
        let initial_branch = if reinitialized {
            head_branch_name_for_outcome(&git_dir)?
        } else {
            resolve_initial_branch(&env, options.initial_branch.as_deref())?
        };

        let separate_layout = options.separate_git_dir.is_some()
            || (!options.bare
                && !options.bare_with_worktree_at_path
                && path.join(".git").is_file());

        let repo = if options.bare_with_worktree_at_path {
            init_bare_with_env_worktree(
                path,
                path,
                &initial_branch,
                options.template_dir.as_deref(),
                options.ref_storage,
                object_format,
            )?
        } else if separate_layout {
            init_repository_separate_git_dir(
                path,
                &git_dir,
                &initial_branch,
                options.template_dir.as_deref(),
                options.ref_storage,
                object_format,
            )?
        } else {
            init_repository_with_object_format(
                path,
                options.bare,
                &initial_branch,
                options.template_dir.as_deref(),
                options.ref_storage,
                object_format,
            )?
        };
        Ok((
            repo,
            InitOutcome {
                reinitialized,
                initial_branch,
            },
        ))
    }

    /// Open or resolve a repository at `path` using `options`.
    ///
    /// When [`RepositoryOptions::open_for_serving`] is set, `path` is resolved like
    /// smart-HTTP serving (work tree, bare root, or `path.git`). Otherwise `path` is
    /// the git directory and [`RepositoryOptions::work_tree`] selects the work tree.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NotARepository`] when the layout is invalid.
    pub fn open(path: &Path, options: &RepositoryOptions) -> Result<Self> {
        if options.open_for_serving {
            return Self::open_for_serving_internal(path, options);
        }
        Self::open_internal(options, path)
    }

    fn open_with_work_tree(
        options: &RepositoryOptions,
        git_dir: &Path,
        work_tree: Option<&Path>,
    ) -> Result<Self> {
        let mut opts = options.clone();
        opts.work_tree = work_tree.map(|p| p.to_path_buf());
        Self::open_internal(&opts, git_dir)
    }

    fn open_internal(options: &RepositoryOptions, git_dir: &Path) -> Result<Self> {
        Self::open_internal_with_refs(options, git_dir, None)
    }

    fn open_internal_with_refs(
        options: &RepositoryOptions,
        git_dir: &Path,
        injected_refs: Option<Arc<dyn crate::refs::store::RefStore>>,
    ) -> Result<Self> {
        crate::trace::trace_span!("grit.repository.open");
        let work_tree = options.work_tree.as_deref();
        let git_dir = git_dir
            .canonicalize()
            .map_err(|_| not_a_repository_at(git_dir))?;

        let repo = Self::from_canonical_git_dir(options, git_dir, work_tree, injected_refs)?;
        repo.refresh_config_session()?;
        if options.validate_repository_format {
            repo.validate_repository_format_cached()?;
        }
        repo.install_config_snapshot();
        warn_core_bare_worktree_conflict(options, repo.git_dir());
        let _ = trace_repo_setup_if_requested(&repo);
        Ok(repo)
    }

    fn open_for_serving_internal(path: &Path, options: &RepositoryOptions) -> Result<Self> {
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
            let mut opts = options.clone();
            opts.work_tree = work_tree;
            opts.open_for_serving = false;
            return Self::open_internal(&opts, &git_dir);
        }
        Err(not_a_repository_at(path))
    }

    fn open_with_odb_internal(
        options: &RepositoryOptions,
        git_dir: &Path,
        mut builder: crate::odb::OdbBuilder,
        injected_refs: Option<Arc<dyn crate::refs::store::RefStore>>,
    ) -> Result<Self> {
        let work_tree = options.work_tree.as_deref();
        let environment = Arc::new(options.environment.clone());
        let cwd = environment.discovery_cwd();
        let command_runner = Arc::clone(&options.command_runner);
        let change_id_generator = Arc::clone(&options.change_id_generator);

        // With both stores injected nothing is read from the git directory's layout, so it
        // may be empty or missing; config and the index are still looked up there.
        let fully_injected = injected_refs.is_some() && builder.has_custom_primary();
        let requested_git_dir = cwd.join(git_dir);
        let git_dir = match requested_git_dir.canonicalize() {
            Ok(dir) => dir,
            Err(_) if fully_injected => canonicalize_existing_prefix(&requested_git_dir)?,
            Err(_) => return Err(not_a_repository_at(git_dir)),
        };

        if injected_refs.is_none() && !head_file_present(&git_dir) {
            return Err(not_a_repository_at(&git_dir));
        }

        let common_git_dir = resolve_common_dir(&git_dir).unwrap_or_else(|| git_dir.clone());
        let objects_dir = resolve_objects_dir(&environment, &git_dir, &common_git_dir);

        // A custom primary does not need objects/ to exist, but the builder must still name it.
        if !builder.has_custom_primary() && !objects_dir.exists() {
            return Err(not_a_repository_at(&git_dir));
        }
        let objects_dir = canonicalize_existing_prefix(&objects_dir)?;
        let builder_objects = canonicalize_existing_prefix(&cwd.join(builder.objects_dir()))?;
        if builder_objects != objects_dir {
            return Err(Error::OdbObjectsDirMismatch {
                builder: builder_objects,
                repository: objects_dir,
            });
        }
        builder.set_objects_dir(objects_dir);

        let work_tree = match work_tree {
            Some(p) => {
                let mut resolved = cwd.join(p);
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

        let ref_store = open_or_install_ref_store(&caches, &git_dir, &environment, injected_refs)?;

        let repo = Self {
            git_dir,
            common_git_dir,
            work_tree,
            odb,
            explicit_git_dir: false,
            discovery_root: None,
            work_tree_from_env: false,
            discovery_via_gitfile: false,
            config_session: Mutex::new(None),
            caches,
            ref_store,
            environment,
            git_prefix,
            command_runner,
            change_id_generator,
            diagnostics: Arc::clone(&options.diagnostics),
            network_trace: options.network_trace,
            reference_unix_time: options.reference_unix_time,
            test_assume_different_owner: options.test_assume_different_owner,
            force_split_index: options.force_split_index,
            thread_policy: options.thread_policy,
            hook_stdio: options.hook_stdio,
        };
        repo.refresh_config_session()?;
        if options.validate_repository_format {
            repo.validate_repository_format_cached()?;
        }
        warn_core_bare_worktree_conflict(options, repo.git_dir());
        let _ = trace_repo_setup_if_requested(&repo);
        Ok(repo)
    }

    /// Discover a repository starting from `start` (defaults to [`Environment::cwd`] when `None`).
    pub fn discover(start: Option<&Path>, options: &RepositoryOptions) -> Result<Self> {
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
                let mut repo = Self::open_with_work_tree(options, &git_dir, work_tree.as_deref())?;
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
            let mut repo = Self::open_with_work_tree(options, &git_dir, resolved_wt.as_deref())?;
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
                repo.environment = Arc::new(env.clone());
                repo.refresh_config_session()?;
                repo.validate_repository_format_cached()?;
                repo.install_config_snapshot();
                let cfg = repo.config()?;
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
                        let (is_bare, core_wt) =
                            read_core_bare_and_worktree_from_config(cfg.as_ref());
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
                let _ = trace_repo_setup_if_requested(&repo);
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

        Err(not_a_repository_at(&start))
    }

    /// Path to the index file.
    #[must_use]
    pub(crate) fn index_path(&self) -> PathBuf {
        self.git_dir.join("index")
    }

    /// Resolve which index file to use, honouring `GIT_INDEX_FILE` like Git plumbing.
    ///
    /// Relative paths are resolved from the process current directory.
    pub(crate) fn index_path_for_env(&self) -> Result<PathBuf> {
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
    pub(crate) fn load_index(&self) -> Result<Index> {
        let path = self.index_path_for_env()?;
        self.load_index_at(&path)
    }

    /// Like [`Repository::load_index`], but reads from an explicit index file path
    /// (e.g. `GIT_INDEX_FILE` or a worktree-specific index).
    pub(crate) fn load_index_at(&self, path: &std::path::Path) -> Result<Index> {
        self.load_index_at_with_context(
            path,
            crate::index::IndexParseContext::from_policy(self.thread_policy()),
        )
    }

    pub(crate) fn load_index_at_with_context(
        &self,
        path: &std::path::Path,
        parse_ctx: crate::index::IndexParseContext,
    ) -> Result<Index> {
        crate::trace::trace_span!("grit.index.read");
        let cfg = self.config().unwrap_or_else(|_| Arc::new(ConfigSet::new()));
        if let Some(res) = cfg.as_ref().get_bool("index.sparse") {
            res.map_err(|s| Error::Config(s.into()))?;
        }
        let mut idx = Index::load_expand_sparse_optional_with_context(
            path,
            &self.odb,
            Some(cfg.as_ref()),
            parse_ctx,
        )?;
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
    pub(crate) fn write_index(&self, index: &mut Index) -> Result<()> {
        self.write_index_with_post_index_change(index, false, false)
    }

    /// Acquire `.git/index.lock` before mutating the working tree for index+worktree commands.
    ///
    /// Call [`Self::commit_index_update`] to persist the index and release the lock, or drop the
    /// lock without committing to abort without writing.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] when the lock file already exists or cannot be created.
    pub(crate) fn begin_index_update(&self) -> Result<crate::index::IndexLock> {
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
    pub(crate) fn commit_index_update(
        &self,
        lock: &mut crate::index::IndexLock,
        index: &mut Index,
        updated_workdir: bool,
    ) -> Result<()> {
        self.flush_index_update(lock, index)?;
        self.finish_index_update(lock, updated_workdir)
    }

    /// Serialize `index` into a held [`crate::index::IndexLock`] without replacing the on-disk index.
    pub(crate) fn flush_index_update(
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
            if !index.suppress_pre_write_index_housekeeping {
                crate::untracked_cache::ensure_untracked_cache_shell(
                    index,
                    work_tree,
                    cfg.as_ref(),
                );
                if !index.skip_racy_smudge_on_write {
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
            }
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
    pub(crate) fn finish_index_update(
        &self,
        lock: &mut crate::index::IndexLock,
        updated_workdir: bool,
    ) -> Result<()> {
        lock.commit_replace_index()?;
        let updated_workdir_arg = if updated_workdir { "1" } else { "0" };
        let _ = run_hook(self, "post-index-change", &[updated_workdir_arg, "0"], None);
        Ok(())
    }

    /// Write a `.bitmap` sidecar for the largest pack in the primary object store.
    ///
    /// # Errors
    ///
    /// Returns [`crate::pack_bitmap::PackBitmapWriteError`] when no pack index exists or the
    /// pack is not closed under reachability from repository refs.
    pub fn write_largest_pack_bitmap(
        &self,
        options: &crate::pack_bitmap::PackBitmapWriteOptions,
        now: std::time::SystemTime,
    ) -> std::result::Result<std::path::PathBuf, crate::pack_bitmap::PackBitmapWriteError> {
        let path = self
            .odb
            .largest_pack_index_path()
            .map_err(|e| crate::pack_bitmap::PackBitmapWriteError::Object(e.to_string()))?
            .ok_or(crate::pack_bitmap::PackBitmapWriteError::InvalidPackPath(
                "no pack index in object store".to_owned(),
            ))?;
        crate::pack_bitmap::PackBitmapWriter::write(self, &path, options, now)
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
    pub(crate) fn try_write_index(&self, index: &mut Index) -> Result<bool> {
        self.try_write_index_at(&self.index_path(), index)
    }

    /// Like [`Repository::try_write_index`], but writes to an explicit index file path
    /// (for example the path selected by `GIT_INDEX_FILE`).
    pub(crate) fn try_write_index_at(&self, path: &Path, index: &mut Index) -> Result<bool> {
        match self.write_index_at(path, index) {
            Ok(()) => Ok(true),
            Err(e) if Self::opportunistic_index_write_skippable(&e) => Ok(false),
            Err(e) => Err(e),
        }
    }

    fn opportunistic_index_write_skippable(err: &crate::error::Error) -> bool {
        use crate::error::Error;
        match err {
            Error::IndexLocked { .. } => true,
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
    pub(crate) fn write_index_with_post_index_change(
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
    pub(crate) fn write_index_at(&self, path: &std::path::Path, index: &mut Index) -> Result<()> {
        self.write_index_at_with_post_index_change(path, index, false, false)
    }

    /// Like [`Repository::write_index_at`], with explicit split-index emission control.
    pub(crate) fn write_index_at_with_split(
        &self,
        path: &std::path::Path,
        index: &mut Index,
        split: WriteSplitIndexRequest,
    ) -> Result<()> {
        self.write_index_at_split(path, index, split)
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
    pub(crate) fn split_index_would_force_write(&self, index: &Index) -> bool {
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
    pub(crate) fn write_index_at_with_post_index_change(
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
    pub(crate) fn write_index_at_split(
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
    pub(crate) fn write_index_at_split_with_post_index_change(
        &self,
        path: &std::path::Path,
        index: &mut Index,
        split: WriteSplitIndexRequest,
        updated_workdir: bool,
        updated_skipworktree: bool,
    ) -> Result<()> {
        self.write_index_file(
            path,
            index,
            split,
            updated_workdir,
            updated_skipworktree,
            None,
        )
    }

    /// Persist `index` through `lock` (from [`Self::begin_index_update`]) and replace the
    /// on-disk index, with the same sparse/split-index handling and `post-index-change` hook as
    /// [`Self::write_index`].
    ///
    /// # Errors
    ///
    /// Propagates index finalization, serialization, and I/O failures. On error the lock is
    /// still held and released when dropped.
    pub(crate) fn write_index_with_lock(
        &self,
        lock: &mut crate::index::IndexLock,
        index: &mut Index,
    ) -> Result<()> {
        let path = lock.index_path.clone();
        self.write_index_file(
            &path,
            index,
            WriteSplitIndexRequest::default(),
            false,
            false,
            Some(lock),
        )
    }

    /// Lock the index selected by `GIT_INDEX_FILE` (or `$GIT_DIR/index`), then load it.
    ///
    /// Read-modify-write commands must load the index only after the lock is held: an index
    /// read before locking can be replaced by a concurrent writer, and writing the stale copy
    /// back would silently drop that writer's update. Persist with
    /// [`Self::write_index_with_lock`]; dropping the lock without writing leaves the index as is.
    ///
    /// Returns the held lock and the index read under it.
    ///
    /// # Errors
    ///
    /// Returns [`Error::IndexLocked`] when another process still holds the lock after the
    /// `core.filesRefLockTimeout` wait, and propagates index read and parse failures
    /// (including [`std::io::ErrorKind::NotFound`] when there is no index file yet).
    pub(crate) fn lock_and_load_index(&self) -> Result<(crate::index::IndexLock, Index)> {
        let path = self.index_path_for_env()?;
        let cfg = self.config()?;
        let lock = crate::index::IndexLock::acquire(&path, Some(cfg.as_ref()))?;
        let index = self.load_index_at(&path)?;
        Ok((lock, index))
    }

    fn write_index_file(
        &self,
        path: &std::path::Path,
        index: &mut Index,
        split: WriteSplitIndexRequest,
        updated_workdir: bool,
        updated_skipworktree: bool,
        existing_lock: Option<&mut crate::index::IndexLock>,
    ) -> Result<()> {
        crate::trace::trace_span!("grit.index.write");
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
            if !index.suppress_pre_write_index_housekeeping {
                crate::untracked_cache::ensure_untracked_cache_shell(
                    index,
                    work_tree,
                    cfg.as_ref(),
                );
                if !index.skip_racy_smudge_on_write {
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
            }
        }
        let skip_hash = crate::index::index_skip_hash_for_write(Some(cfg.as_ref()));
        write_index_file_split(
            path,
            &self.git_dir,
            index,
            cfg.as_ref(),
            split,
            skip_hash,
            self.thread_policy(),
            existing_lock,
        )?;
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
        // Checkout writes the index before moving HEAD; a valid cache-tree root names the tree
        // the index already matches.
        let tree_oid = match index.cache_tree_root {
            Some(tree) => Some(tree),
            None => match resolve_head(&self.git_dir)?.oid() {
                Some(oid) => Some(parse_commit(&self.odb.read(oid)?.data)?.tree),
                None => None,
            },
        };
        if let Some(t) = tree_oid {
            let protect = crate::path_validate::ProtectOptions::from_config(cfg.as_ref());
            index.try_collapse_sparse_directories(
                &self.odb, &t, &patterns, cone, sparse_ix, protect,
            )?;
        } else {
            index.sparse_directories = false;
        }
        Ok(())
    }

    /// Relative path from the work tree root to the process current directory, `/`-separated.
    ///
    /// Used for `:(top)` / `:/` pathspec Bloom lookups. Returns `None` for bare repositories or
    /// when paths cannot be resolved; callers should treat `None` like an empty prefix.
    #[must_use]
    pub(crate) fn bloom_pathspec_cwd(&self) -> Option<String> {
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

    /// Ref prefix holding replace refs (`refs/replace/` unless `GIT_REPLACE_REF_BASE` says
    /// otherwise), or `None` when `GIT_NO_REPLACE_OBJECTS` or `core.useReplaceRefs=false`
    /// disables them.
    pub(crate) fn replace_ref_base_if_enabled(&self) -> Option<String> {
        if self.environment.git_no_replace_objects {
            return None;
        }
        let settings = self.cached_settings();
        settings
            .use_replace_refs
            .then_some(settings.replace_ref_base)
    }

    /// Object id to use after applying `refs/replace` (when enabled).
    pub(crate) fn object_id_after_replace(
        &self,
        oid: &crate::objects::ObjectId,
    ) -> crate::objects::ObjectId {
        self.resolve_replace_redirect(oid).unwrap_or(*oid)
    }

    /// Metadata for an object, following replace refs without reading the payload.
    pub(crate) fn read_info_replaced(
        &self,
        oid: &crate::objects::ObjectId,
    ) -> Result<crate::objects::ObjectInfo> {
        let effective = self.object_id_after_replace(oid);
        self.odb.read_info(&effective)
    }

    /// Read an object, transparently following replace refs.
    ///
    /// If `refs/replace/<hex>` exists for the requested OID and
    /// `GIT_NO_REPLACE_OBJECTS` is **not** set, this reads the
    /// replacement object instead.  Otherwise it behaves identically
    /// to `self.odb.read(oid)`.
    pub(crate) fn read_replaced(
        &self,
        oid: &crate::objects::ObjectId,
    ) -> Result<crate::objects::Object> {
        let effective = self.object_id_after_replace(oid);
        self.odb.read(&effective)
    }

    fn resolve_replace_redirect(
        &self,
        oid: &crate::objects::ObjectId,
    ) -> Option<crate::objects::ObjectId> {
        if self.environment.git_no_replace_objects {
            return None;
        }
        if let Ok(guard) = self.config_session.lock() {
            if guard.as_ref().is_some_and(|s| s.replace_lookup_inactive) {
                return None;
            }
        }
        let settings = self.cached_settings();
        if !settings.use_replace_refs {
            return None;
        }
        let replace_ref =
            self.common_git_dir
                .join(format!("{}{}", settings.replace_ref_base, oid.to_hex()));
        if !replace_ref.is_file() {
            return None;
        }
        let content = std::fs::read_to_string(&replace_ref).ok()?;
        let hex = content.trim();
        let replacement_oid = hex.parse::<crate::objects::ObjectId>().ok()?;
        if self.odb.read_info(&replacement_oid).is_ok() {
            Some(replacement_oid)
        } else {
            None
        }
    }
}

/// If `GIT_TRACE_SETUP` is an absolute path, append `setup:` lines (Git test format).
///
/// Upstream tests grep `^setup: ` from the trace file; they do not use the timestamped
/// `trace.c:` prefix that full Git tracing adds.
pub(crate) fn trace_repo_setup_if_requested(repo: &Repository) -> std::io::Result<()> {
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
fn replace_dir_is_non_empty(replace_dir: &Path) -> bool {
    let Ok(entries) = fs::read_dir(replace_dir) else {
        return false;
    };
    entries.filter_map(|e| e.ok()).any(|e| {
        let Ok(ft) = e.file_type() else {
            return false;
        };
        ft.is_file()
    })
}

/// Resolve the common git directory for linked worktrees.
fn resolve_common_dir(git_dir: &Path) -> Option<PathBuf> {
    let commondir_file = git_dir.join("commondir");
    if !commondir_file.is_file() {
        return None;
    }
    let common_raw = fs::read_to_string(commondir_file).ok()?;
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
    let Ok(content) = crate::repo_caches::read_local_config_file(&path) else {
        return false;
    };
    worktree_config_enabled_for_text(Some(content.as_str()))
}

/// Like [`worktree_config_enabled`] when `config` body is already in memory.
pub fn worktree_config_enabled_for_text(content: Option<&str>) -> bool {
    let Some(content) = content else {
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
    let path = common_dir.join("config");
    let content = crate::repo_caches::read_local_config_file(&path).ok()?;
    early_config_ignore_repo_reason_for_text(Some(content.as_str()))
}

/// Like [`early_config_ignore_repo_reason`] when `config` body is already in memory.
pub fn early_config_ignore_repo_reason_for_text(content: Option<&str>) -> Option<String> {
    const GIT_REPO_VERSION_READ: u32 = 1;
    let content = content?;
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

/// Path to the repository-local config file (`.git/config`, or `commondir/config` for a linked
/// worktree admin directory that has no separate `config` file).
///
/// Used when writing `git config --local` semantics from library code.
pub fn local_config_write_path(git_dir: &Path) -> PathBuf {
    if let Some(path) = repository_config_path(git_dir) {
        return path;
    }
    if let Some(common) = resolve_common_dir(git_dir) {
        return common.join("config");
    }
    git_dir.join("config")
}

/// Determine the config file path for a repository or linked worktree.
fn repository_config_path(git_dir: &Path) -> Option<PathBuf> {
    repository_config_path_for_caches(git_dir)
}

/// Same as [`repository_config_path`] for [`crate::repo_caches::RepoCaches`].
pub(crate) fn repository_config_path_for_caches(git_dir: &Path) -> Option<PathBuf> {
    let local = git_dir.join("config");
    if local.is_file() {
        return Some(local);
    }
    let commondir = git_dir.join("commondir");
    if !commondir.is_file() {
        return None;
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
        crate::ref_storage::RefStorageFormat::validate_config_extension(raw)?;
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
    let content = crate::repo_caches::read_local_config_file(&config_path)?;
    let parsed = parse_repository_format(&content, &config_path)?;
    validate_repository_format_parsed(&parsed)
}

/// The result of parsing `core.repositoryformatversion` and `extensions.*` from a
/// repository's `config` file, using Git-compatible format parsing.
#[derive(Clone)]
pub(crate) struct RepositoryFormat {
    /// Declared `core.repositoryformatversion` (defaults to 0; invalid values ignored).
    pub(crate) repo_version: u32,
    /// All extension keys (lowercased) declared under `[extensions]`.
    pub(crate) extensions: BTreeSet<String>,
    /// Raw value of `extensions.refstorage`, if present.
    pub(crate) ref_storage: Option<String>,
    /// Raw value of `extensions.objectformat`, if present.
    pub(crate) object_format: Option<String>,
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
            object_format: None,
        });
    };
    let content = crate::repo_caches::read_local_config_file(&config_path)?;
    parse_repository_format(&content, &config_path)
}

impl RepositoryFormat {
    /// Object hash algorithm declared by `extensions.objectformat`, SHA-1 when absent or
    /// unrecognized (the same fallback as [`crate::odb::hash_algo_for_git_dir`]).
    pub(crate) fn hash_algo(&self) -> crate::objects::HashAlgo {
        self.object_format
            .as_deref()
            .and_then(crate::objects::HashAlgo::from_name)
            .unwrap_or(crate::objects::HashAlgo::Sha1)
    }

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
pub(crate) fn parse_repository_format(
    content: &str,
    config_path: &Path,
) -> Result<RepositoryFormat> {
    let mut in_core = false;
    let mut in_extensions = false;
    let mut repo_version = 0u32;
    let mut extensions = BTreeSet::new();
    let mut ref_storage: Option<String> = None;
    let mut object_format: Option<String> = None;

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
            if key.eq_ignore_ascii_case("objectformat") {
                object_format = value.map(str::to_owned);
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
        object_format,
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
                    path_for_error_message(&dot_git)
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
                            path_for_error_message(&dot_git)
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
        let mut opts = options.clone();
        opts.validate_repository_format = false;
        opts.work_tree = Some(dir.to_path_buf());
        let mut repo = Repository::open_internal(&opts, &git_dir)?;
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
        let mut skip_opts = options.clone();
        skip_opts.validate_repository_format = false;
        skip_opts.work_tree = Some(dir.to_path_buf());
        match Repository::open_internal(&skip_opts, &open_path) {
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
            Err(e) => return Err(e),
        }
    }

    // Linked-worktree gitdir/admin directories contain HEAD and commondir,
    // and can be opened as repositories even without a local objects/ dir.
    if dir.join("HEAD").is_file() && dir.join("commondir").is_file() {
        maybe_trace_implicit_bare_repository(dir, env);
        let repo = Repository::open_with_work_tree(options, dir, None)?;
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
                        return Err(Error::ForbiddenBareRepository(path_for_error_message(dir)));
                    }
                }
            }
        }
        let repo = Repository::open_with_work_tree(options, dir, None)?;
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
    Err(Error::DubiousOwnership(path_for_error_message(checked)))
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
    pub(crate) fn enforce_safe_directory_git_dir(&self) -> Result<()> {
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
            .ok_or_else(|| not_a_repository_at(git_dir))?;
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
            .map_err(|_| not_a_repository_at(dot_git));
    }
    if dot_git.is_file() {
        let content =
            fs::read_to_string(dot_git).map_err(|e| Error::NotARepository(e.to_string()))?;
        let base = dot_git
            .parent()
            .ok_or_else(|| not_a_repository_at(dot_git))?;
        return parse_gitfile(&content, base);
    }
    Err(not_a_repository_at(dot_git))
}

fn not_a_repository_at(path: &Path) -> Error {
    Error::NotARepository(path_for_error_message(path))
}

/// Canonicalize the longest existing prefix of `path` and append the rest unchanged, so a
/// path that does not exist yet is spelled the same way as its existing parent (through
/// symlinks such as macOS `/var` -> `/private/var`).
fn canonicalize_existing_prefix(path: &Path) -> Result<PathBuf> {
    let absolute = std::path::absolute(path)?;
    let mut missing = Vec::new();
    let mut existing = absolute.as_path();
    loop {
        if let Ok(canonical) = existing.canonicalize() {
            return Ok(missing
                .iter()
                .rev()
                .fold(canonical, |acc: PathBuf, part| acc.join(part)));
        }
        match (existing.parent(), existing.file_name()) {
            (Some(parent), Some(name)) => {
                missing.push(name.to_owned());
                existing = parent;
            }
            _ => return Ok(absolute),
        }
    }
}

/// The repository's primary objects directory.
///
/// [`Environment::git_object_directory`] wins when set and non-empty (a relative path resolves
/// against [`Environment::discovery_cwd`]); the main store is then reachable only through
/// alternates, as in Git. Otherwise `objects/` in `git_dir`, or in `common_git_dir` for a
/// linked worktree's admin directory.
fn resolve_objects_dir(
    environment: &Environment,
    git_dir: &Path,
    common_git_dir: &Path,
) -> PathBuf {
    if let Some(dir) = environment
        .git_object_directory
        .as_deref()
        .filter(|dir| !dir.is_empty())
    {
        return environment.discovery_cwd().join(dir);
    }
    let own = git_dir.join("objects");
    if own.exists() {
        own
    } else {
        common_git_dir.join("objects")
    }
}

fn head_file_present(git_dir: &Path) -> bool {
    let head_path = git_dir.join("HEAD");
    head_path.exists() || head_path.is_symlink()
}

/// Use `injected` as the repository's ref store (registering it for `git_dir`), or open the
/// backend the git directory's config selects.
fn open_or_install_ref_store(
    caches: &RepoCaches,
    git_dir: &Path,
    environment: &Environment,
    injected: Option<Arc<dyn crate::refs::store::RefStore>>,
) -> Result<Arc<dyn crate::refs::store::RefStore>> {
    match injected {
        Some(store) => {
            caches.install_ref_store(git_dir, Arc::clone(&store));
            Ok(store)
        }
        None => caches.open_ref_store(git_dir, environment),
    }
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
                return Err(not_a_repository_at(&path));
            }
            return Ok(path);
        }
    }
    Err(Error::NotARepository("invalid gitfile format".to_owned()))
}

/// Resolve the initial branch name for a new repository.
///
/// When `explicit` is `Some`, that name is validated and returned. Otherwise
/// `init.defaultBranch` is read from the configuration cascade (global/system,
/// not any repository-local config) and falls back to `main`.
///
/// # Errors
///
/// Returns [`Error::InvalidRef`] when the branch name is not a valid short branch name.
pub fn resolve_initial_branch(env: &Environment, explicit: Option<&str>) -> Result<String> {
    let name = match explicit {
        Some(name) => name.to_owned(),
        None => ConfigSet::load(env, None, true)?
            .get_last_entry("init.defaultBranch")
            .and_then(|e| e.value.clone())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "main".to_owned()),
    };
    validate_branch_short_name(&name).map_err(|e| Error::InvalidRef(e.to_string()))?;
    Ok(name)
}

fn resolve_git_dir_for_init(path: &Path, options: &InitOptions) -> Result<PathBuf> {
    if options.bare_with_worktree_at_path || options.bare {
        return Ok(path.to_path_buf());
    }
    if let Some(separate) = &options.separate_git_dir {
        return Ok(separate.clone());
    }
    let dot_git = path.join(".git");
    if dot_git.is_file() {
        let content = fs::read_to_string(&dot_git)?;
        return parse_gitfile(&content, path);
    }
    Ok(dot_git)
}

/// Branch short name for init outcome reporting; tolerates detached `HEAD` on reinit.
fn head_branch_name_for_outcome(git_dir: &Path) -> Result<String> {
    if let Some(target) = crate::refs::read_symbolic_ref(git_dir, "HEAD")? {
        if let Some(short) = target.strip_prefix("refs/heads/") {
            return Ok(short.to_owned());
        }
        return Ok(target
            .rsplit('/')
            .next()
            .unwrap_or(target.as_str())
            .to_owned());
    }
    let head_text = fs::read_to_string(git_dir.join("HEAD"))?;
    let oid = head_text.trim();
    if crate::objects::ObjectId::from_hex(oid).is_ok() {
        let end = oid.len().min(7);
        return Ok(oid[..end].to_owned());
    }
    Ok("HEAD".to_owned())
}

fn hash_algo_from_git_dir(git_dir: &Path) -> Result<HashAlgo> {
    let config_path = git_dir.join("config");
    if !config_path.is_file() {
        return Ok(HashAlgo::Sha1);
    }
    let content = fs::read_to_string(&config_path)?;
    Ok(parse_repository_format(&content, &config_path)?.hash_algo())
}

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
    object_format: HashAlgo,
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

    let needs_extensions = ref_storage.is_reftable() || object_format != HashAlgo::Sha1;
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
        if ref_storage.is_reftable() {
            config_content.push_str("\trefStorage = reftable\n");
        }
        if object_format != HashAlgo::Sha1 {
            config_content.push_str(&format!("\tobjectformat = {}\n", object_format.name()));
        }
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
pub(crate) fn init_repository_separate_git_dir(
    work_tree: &Path,
    git_dir: &Path,
    initial_branch: &str,
    template_dir: Option<&Path>,
    ref_storage: crate::ref_storage::RefStorageFormat,
    object_format: HashAlgo,
) -> Result<Repository> {
    let skip_hooks_info = template_dir.is_some_and(|p| p.as_os_str().is_empty());
    fs::create_dir_all(work_tree)?;
    fs::create_dir_all(git_dir)?;
    let existing = git_dir.join("config").is_file() && git_dir.join("HEAD").is_file();
    if existing {
        ensure_git_directory_layout(git_dir, false, ref_storage, skip_hooks_info)?;
    } else {
        write_fresh_git_directory(
            git_dir,
            false,
            initial_branch,
            template_dir,
            ref_storage,
            object_format,
            skip_hooks_info,
        )?;
    }

    // Write an absolute `gitdir:` path, matching C Git's `init_db` →
    // `set_git_dir(real_git_dir, make_realpath=1)` → `separate_git_dir`, which records the
    // realpath of the separate git directory (`t5601` "clone separate gitdir: output"). This
    // path is only used by `git clone --separate-git-dir`; submodule layouts use a different
    // code path and are unaffected.
    let gitfile = work_tree.join(".git");
    let abs_git_dir = fs::canonicalize(git_dir).unwrap_or_else(|_| git_dir.to_path_buf());
    let abs_git_dir = abs_git_dir.to_string_lossy().replace('\\', "/");
    fs::write(gitfile, format!("gitdir: {abs_git_dir}\n"))?;

    Repository::open(
        git_dir,
        &RepositoryOptions::empty().with_work_tree(work_tree),
    )
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

pub(crate) fn init_repository(
    path: &Path,
    bare: bool,
    initial_branch: &str,
    template_dir: Option<&Path>,
    ref_storage: crate::ref_storage::RefStorageFormat,
) -> Result<Repository> {
    init_repository_with_object_format(
        path,
        bare,
        initial_branch,
        template_dir,
        ref_storage,
        HashAlgo::Sha1,
    )
}

pub(crate) fn init_repository_with_object_format(
    path: &Path,
    bare: bool,
    initial_branch: &str,
    template_dir: Option<&Path>,
    ref_storage: crate::ref_storage::RefStorageFormat,
    object_format: HashAlgo,
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
            object_format,
            skip_hooks_info,
        )?;
    }

    if bare {
        Repository::open(&git_dir, &RepositoryOptions::empty())
    } else {
        Repository::open(
            &git_dir,
            &RepositoryOptions::empty().with_work_tree(path.to_path_buf()),
        )
    }
}

/// Initialise a **bare** repository at `git_dir` with `core.worktree` set to `work_tree`.
///
/// Used when `GIT_WORK_TREE` is set during `git clone`: the clone destination is the bare
/// git directory and checked-out files go under the environment work tree (matches upstream Git).
///
/// # Errors
///
/// Returns [`Error::Io`] on filesystem failures.
pub(crate) fn init_bare_with_env_worktree(
    git_dir: &Path,
    work_tree: &Path,
    initial_branch: &str,
    template_dir: Option<&Path>,
    ref_storage: crate::ref_storage::RefStorageFormat,
    object_format: HashAlgo,
) -> Result<Repository> {
    fs::create_dir_all(git_dir)?;
    fs::create_dir_all(work_tree)?;
    let existing = git_dir.join("config").is_file() && git_dir.join("HEAD").is_file();
    if existing {
        ensure_git_directory_layout(git_dir, true, ref_storage, false)?;
    } else {
        write_fresh_git_directory(
            git_dir,
            true,
            initial_branch,
            template_dir,
            ref_storage,
            object_format,
            false,
        )?;
    }
    let work_tree_abs = fs::canonicalize(work_tree).unwrap_or_else(|_| work_tree.to_path_buf());
    let config_path = git_dir.join("config");
    let mut config = match ConfigFile::from_path(&config_path, ConfigScope::Local)? {
        Some(c) => c,
        None => ConfigFile::parse(&config_path, "", ConfigScope::Local)?,
    };
    config.set("core.worktree", &work_tree_abs.to_string_lossy())?;
    config.write()?;
    Repository::open(
        git_dir,
        &RepositoryOptions::empty().with_work_tree(work_tree),
    )
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

/// Per-repository caches shared across open handles.
pub mod caches {
    pub use crate::repo_caches::*;
}

mod index_write_fail_inject {
    use std::cell::RefCell;

    // hygiene: thread-local index write failure injection for tests
    thread_local! {
        pub static INJECT: RefCell<bool> = const { RefCell::new(false) };
        pub static INJECT_REMAINING: RefCell<Option<usize>> = const { RefCell::new(None) };
    }
}

/// Debug-only: fail every [`Repository::write_index`] / index lock path while enabled.
#[doc(hidden)]
pub fn set_test_inject_index_write_fail(enabled: bool) {
    index_write_fail_inject::INJECT.with(|c| *c.borrow_mut() = enabled);
    if !enabled {
        index_write_fail_inject::INJECT_REMAINING.with(|c| *c.borrow_mut() = None);
    }
}

/// Debug-only: fail the next `remaining` index write attempts, then allow writes.
#[doc(hidden)]
pub fn set_test_inject_index_write_fail_remaining(remaining: usize) {
    index_write_fail_inject::INJECT.with(|c| *c.borrow_mut() = true);
    index_write_fail_inject::INJECT_REMAINING.with(|c| *c.borrow_mut() = Some(remaining));
}

fn test_inject_index_write_fail() -> Result<()> {
    index_write_fail_inject::INJECT.with(|enabled| {
        if !*enabled.borrow() {
            return Ok(());
        }
        index_write_fail_inject::INJECT_REMAINING.with(|remaining| {
            if let Some(left) = remaining.borrow_mut().as_mut() {
                if *left == 0 {
                    return Ok(());
                }
                *left -= 1;
            }
            Err(Error::Io(std::io::Error::other(
                "injected index write failure",
            )))
        })
    })
}

#[cfg(test)]
#[path = "repo_line_coverage_tests.rs"]
mod line_coverage_tests;
