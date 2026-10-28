//! Clone a remote repository: initialize, configure `origin`, fetch, set up the default branch,
//! and optionally check out the working tree.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use thiserror::Error;

use crate::config::{ConfigFile, ConfigScope, ConfigSet};
use crate::environment::Environment;
use crate::environment::RepositoryOptions;
use crate::error::{Error, Result};
use crate::fetch::{fetch_operation_identity, Progress};
use crate::objects::{HashAlgo, ObjectId};
use crate::partial_clone::{ObjectFilterSpec, PartialCloneConfig};
use crate::progress::Cancel;
use crate::promisor_fetch::PromisorFetcher;
use crate::ref_storage::RefStorageFormat;
use crate::refs;
use crate::remote::{
    fold_peeled_refs, HttpClientFactory, ListRefsOptions, Remote, RemoteError, DEFAULT_REMOTE,
};
use crate::repo::{init_repository_with_object_format, Repository};
use crate::transfer::{CloneReflog, FetchOptions, TagMode, TransferStats};
use crate::transport_path::{
    absolute_local_clone_source_url, git_url_basename, resolve_local_remote_git_dir,
    should_store_absolute_local_clone_url, url_is_local_not_ssh, NoDirectoryName,
};

/// Errors during [`clone`].
#[derive(Debug, Error)]
pub enum CloneError {
    /// Destination exists and is not empty.
    ///
    /// Grit does not create or modify the destination in this case; callers must not run
    /// [`cleanup_failed_clone`] on the path.
    #[error("destination '{0}' already exists and is not empty")]
    DestNotEmpty(String),
    /// Named branch or tag from [`CloneOptions::branch`] was not advertised by the remote.
    #[error("remote ref '{0}' not found")]
    RemoteRefNotFound(String),
    /// [`CloneOptions::depth`] was zero (`git clone --depth 0` is invalid).
    #[error("depth 0 is not a positive number")]
    InvalidCloneDepth,
    /// Underlying library failure.
    #[error(transparent)]
    Library(#[from] Error),
    /// Remote resolution or transport failure.
    #[error(transparent)]
    Remote(#[from] crate::remote::RemoteError),
}

impl CloneError {
    /// Whether [`cleanup_failed_clone`] may run after this failure.
    ///
    /// Returns `false` when the destination must be left unchanged: [`CloneError::DestNotEmpty`]
    /// and pre-initialization failures such as [`RemoteError::InvalidRemoteName`].
    #[must_use]
    pub fn should_cleanup_failed_clone(&self) -> bool {
        match self {
            Self::DestNotEmpty(_) | Self::InvalidCloneDepth => false,
            Self::Remote(RemoteError::InvalidRemoteName(_)) => false,
            Self::RemoteRefNotFound(_) => true,
            Self::Library(_) | Self::Remote(_) => true,
        }
    }

    /// Whether the failure is a missing `-b` / `--branch` ref (destination may be removed).
    #[must_use]
    pub fn is_remote_ref_not_found(&self) -> bool {
        matches!(self, Self::RemoteRefNotFound(_))
    }

    /// Whether the source could not be read at all ([`Error::RemoteUnreadable`]), as opposed to
    /// an empty remote with no branches or a local failure.
    #[must_use]
    pub fn is_remote_unreadable(&self) -> bool {
        match self {
            Self::Library(e) => e.is_remote_unreadable(),
            Self::Remote(e) => e.is_remote_unreadable(),
            Self::DestNotEmpty(_) | Self::RemoteRefNotFound(_) | Self::InvalidCloneDepth => false,
        }
    }

    /// Whether the clone was cancelled ([`Error::Cancelled`]).
    ///
    /// When this returns `true`, [`Self::should_cleanup_failed_clone`] is also `true`.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        match self {
            Self::Library(e) => e.is_cancelled(),
            Self::Remote(e) => e.is_cancelled(),
            Self::DestNotEmpty(_) | Self::RemoteRefNotFound(_) | Self::InvalidCloneDepth => false,
        }
    }
}

/// Which tags to fetch during [`clone`], matching `git clone` tag behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloneTags {
    /// Fetch every tag on the remote (`git fetch --tags` during clone).
    All,
    /// Fetch tags reachable from fetched commits (default for shallow clones).
    Following,
    /// Do not fetch tags; writes `remote.<name>.tagOpt=--no-tags`.
    None,
}

impl CloneTags {
    /// Maps to [`TagMode`] for the initial fetch.
    #[must_use]
    pub const fn tag_mode(self) -> TagMode {
        match self {
            Self::All => TagMode::All,
            Self::Following => TagMode::Following,
            Self::None => TagMode::None,
        }
    }
}

/// Options for [`clone`].
pub struct CloneOptions {
    /// Source URL or path (as passed on the command line).
    pub url: String,
    /// Directory to create or use for the new repository.
    pub dest: PathBuf,
    /// Remote name to configure (default [`DEFAULT_REMOTE`]).
    pub remote_name: String,
    /// Process environment for config and reflog identity.
    pub environment: Environment,
    /// Initial branch name used when initializing the empty repo (`main`).
    pub initial_branch: String,
    /// Global thread policy for fetch pack ingest and the opened repository (`--threads`).
    pub thread_policy: crate::parallel::ThreadPolicy,
    /// Truncate fetched history to this many commits per ref tip
    /// (`git clone --depth N`). `Some(1)` matches a shallow clone.
    pub depth: Option<u32>,
    /// Partial-clone object filter applied to the initial fetch and recorded in config.
    pub filter: Option<ObjectFilterSpec>,
    /// Remote branch or tag to check out (`git clone -b` / `--branch`).
    pub branch: Option<String>,
    /// Single-branch fetch (`git clone --single-branch` / `--no-single-branch`).
    ///
    /// `None` matches Git: depth implies single-branch unless explicitly disabled.
    pub single_branch: Option<bool>,
    /// Tag fetch mode. `None` uses Git defaults: all tags for full multi-branch clones,
    /// tag-following when [`clone_effective_single_branch`] is true (explicit
    /// `--single-branch` or shallow clone without `--no-single-branch`).
    pub tags: Option<CloneTags>,
    /// Optional on-demand promisor fetcher installed on the opened repository (for checkout prefetch).
    pub promisor_fetcher: Option<Arc<dyn PromisorFetcher>>,
    /// Create a bare repository (`git clone --bare`).
    pub bare: bool,
    /// Populate the index and working tree after fetch (`git clone --no-checkout` when false).
    pub checkout: bool,
    /// Where bundle downloads are kept, so an interrupted clone resumes its
    /// download next time. `None` skips the server's `bundle-uri` bundle. Shallow
    /// and partial clones never use one.
    pub bundle_cache: Option<PathBuf>,
}

impl CloneOptions {
    /// Options for cloning into `dest` from `url`, using the current process environment.
    #[must_use]
    pub fn new(url: impl Into<String>, dest: PathBuf) -> Self {
        Self {
            url: url.into(),
            dest,
            remote_name: DEFAULT_REMOTE.to_owned(),
            environment: Environment::capture_process(), // hygiene: CLI boundary — snapshot process environment for clone
            initial_branch: "main".to_owned(),
            thread_policy: crate::parallel::ThreadPolicy::default(),
            depth: None,
            filter: None,
            branch: None,
            single_branch: None,
            tags: None,
            promisor_fetcher: None,
            bare: false,
            checkout: true,
            bundle_cache: None,
        }
    }
}

/// Initial checkout disposition after [`clone`] (before working-tree checkout).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CloneHead {
    /// Checked out on a local branch that tracks the remote.
    Branch(String),
    /// Detached at a tag's peeled commit (`git clone -b <tag>`).
    Detached {
        /// Tag name from `-b` / `--branch`.
        tag: String,
        /// Peeled commit object id.
        oid: ObjectId,
    },
}

impl CloneHead {
    /// Local branch name when [`CloneHead::Branch`]; `None` when detached.
    #[must_use]
    pub fn branch_name(&self) -> Option<&str> {
        match self {
            Self::Branch(name) => Some(name.as_str()),
            Self::Detached { .. } => None,
        }
    }

    /// Tag name when detached at a tag; `None` on a branch checkout.
    #[must_use]
    pub fn tag_name(&self) -> Option<&str> {
        match self {
            Self::Detached { tag, .. } => Some(tag.as_str()),
            Self::Branch(_) => None,
        }
    }

    /// Whether `HEAD` is detached (tag checkout).
    #[must_use]
    pub fn is_detached(&self) -> bool {
        matches!(self, Self::Detached { .. })
    }
}

/// Whether [`clone`] checked out the default branch or left the work tree empty.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloneCheckout {
    /// The default branch tip was fetched and should be checked out.
    CheckedOut(ObjectId),
    /// No objects and no remote branches were received.
    EmptyRemote,
    /// The remote advertised default branch has no commits yet; other refs may exist.
    UnbornDefaultBranch,
    /// A commit was fetched but the working tree was not checked out (`--bare` or `--no-checkout`).
    Skipped,
}

/// Result of a successful [`clone`].
#[derive(Debug)]
pub struct CloneOutcome {
    /// Source URL string from the options.
    pub url: String,
    /// Destination path.
    pub path: PathBuf,
    /// Branch checked out, or detached tag checkout.
    pub head: CloneHead,
    /// Commit to check out on that branch.
    ///
    /// [`ObjectId::zero()`] when the remote default branch has no commits yet.
    pub checkout_oid: ObjectId,
    /// Checkout disposition after fetch and default-branch setup.
    pub checkout: CloneCheckout,
    /// Remote-tracking branch names that resolved to a commit (empty unless checkout is
    /// [`CloneCheckout::UnbornDefaultBranch`]).
    pub branches_with_commits: Vec<String>,
    /// Open handle to the new repository.
    pub repo: Repository,
    /// Objects and pack bytes received during the initial fetch.
    pub transfer: TransferStats,
    /// Remote branches fetched (from the fetch ref updates).
    pub branches: u64,
    /// Remote tags fetched (from the fetch ref updates).
    pub tags: u64,
    /// Working-tree files checked out (zero when checkout was skipped or not applicable).
    pub files: u64,
    /// Total size of checked-out files in bytes.
    pub worktree_bytes: u64,
    /// Index paths that collided on a case-insensitive filesystem during checkout.
    pub path_collisions: Vec<String>,
    /// Gitlink paths materialized during checkout (submodules grit does not fetch).
    pub submodule_paths: Vec<String>,
    /// Whether the destination is a bare repository.
    pub bare: bool,
    /// Whether a bundle from the server's `bundle-uri` seeded the clone.
    /// [`Self::transfer`] includes what the bundle brought.
    pub bundle: crate::bundle_uri::BundleSeed,
    /// True when the initial fetch sent an object filter and the server applied it.
    pub filter_sent: bool,
}

/// Derive a destination directory from a clone URL (Git [`git_url_basename`]).
///
/// # Errors
///
/// Returns [`NoDirectoryName`] when no directory name can be guessed (same as `git clone`
/// without `<dir>`).
pub fn derive_clone_dir(url: &str, bare: bool) -> std::result::Result<String, NoDirectoryName> {
    git_url_basename(url, false, bare)
}

/// Whether `path` is an existing empty directory (used for failed-clone cleanup policy).
#[must_use]
pub fn dest_preexisted_empty(path: &Path) -> bool {
    path.is_dir()
        && path
            .read_dir()
            .map(|mut entries| entries.next().is_none())
            .unwrap_or(false)
}

struct CloneDestinationGuard;

impl CloneDestinationGuard {
    fn try_claim(dest: &Path, bare: bool) -> std::result::Result<(), CloneError> {
        let label = dest
            .to_str()
            .ok_or_else(|| {
                CloneError::Library(Error::Message("non-UTF-8 destination path".into()))
            })?
            .to_owned();
        if bare {
            Self::try_claim_bare(dest, label)
        } else {
            Self::try_claim_non_bare(dest, label)
        }
    }

    fn try_claim_non_bare(dest: &Path, label: String) -> std::result::Result<(), CloneError> {
        if dest.is_dir() && !dir_is_empty(dest) {
            return Err(CloneError::DestNotEmpty(label));
        }

        if !dest.is_dir() {
            if let Some(parent) = dest.parent() {
                fs::create_dir_all(parent).map_err(|e| {
                    CloneError::Library(Error::Message(format!(
                        "could not create clone destination parent: {e}"
                    )))
                })?;
            }
            match fs::create_dir(dest) {
                Ok(()) => {}
                Err(source) if source.kind() == io::ErrorKind::AlreadyExists => {}
                Err(source) => {
                    return Err(CloneError::Library(Error::Message(format!(
                        "could not create clone destination: {source}"
                    ))));
                }
            }
        }

        if !dest.is_dir() {
            return Err(CloneError::DestNotEmpty(label));
        }
        if !dir_is_empty(dest) {
            return Err(CloneError::DestNotEmpty(label));
        }

        let git_dir = dest.join(".git");
        match fs::create_dir(&git_dir) {
            Ok(()) => Ok(()),
            Err(source) if source.kind() == io::ErrorKind::AlreadyExists => {
                Err(CloneError::DestNotEmpty(label))
            }
            Err(source) => Err(CloneError::Library(Error::Message(format!(
                "could not create '{}/.git': {source}",
                dest.display()
            )))),
        }
    }

    fn try_claim_bare(dest: &Path, label: String) -> std::result::Result<(), CloneError> {
        if dest.is_dir() && !dir_is_empty(dest) {
            return Err(CloneError::DestNotEmpty(label));
        }

        if !dest.is_dir() {
            if let Some(parent) = dest.parent() {
                fs::create_dir_all(parent).map_err(|e| {
                    CloneError::Library(Error::Message(format!(
                        "could not create clone destination parent: {e}"
                    )))
                })?;
            }
            match fs::create_dir(dest) {
                Ok(()) => {}
                Err(source) if source.kind() == io::ErrorKind::AlreadyExists => {}
                Err(source) => {
                    return Err(CloneError::Library(Error::Message(format!(
                        "could not create clone destination: {source}"
                    ))));
                }
            }
        }

        if !dest.is_dir() {
            return Err(CloneError::DestNotEmpty(label));
        }
        if !dir_is_empty(dest) {
            return Err(CloneError::DestNotEmpty(label));
        }

        // Reserve the bare git directory before concurrent inits race on `config` / `HEAD`.
        let objects = dest.join("objects");
        match fs::create_dir(&objects) {
            Ok(()) => Ok(()),
            Err(source) if source.kind() == io::ErrorKind::AlreadyExists => {
                Err(CloneError::DestNotEmpty(label))
            }
            Err(source) => Err(CloneError::Library(Error::Message(format!(
                "could not create '{}/objects': {source}",
                dest.display()
            )))),
        }
    }
}

fn dir_is_empty(path: &Path) -> bool {
    path.read_dir()
        .map(|mut entries| entries.next().is_none())
        .unwrap_or(false)
}

/// Remove a partially created clone destination.
///
/// When `keep_toplevel` is true, only contents of `path` are removed (empty dir existed before clone).
/// Call it on the thread that ran [`clone`]: it first drops that thread's cached pack maps.
pub fn cleanup_failed_clone(path: &Path, keep_toplevel: bool) {
    // Windows refuses to delete a memory-mapped file, and the clone's pack reads leave the
    // new packs mapped in this thread's caches.
    crate::pack::clear_pack_cache();
    if keep_toplevel {
        let Ok(entries) = fs::read_dir(path) else {
            return;
        };
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                let _ = fs::remove_dir_all(p);
            } else {
                let _ = fs::remove_file(p);
            }
        }
    } else {
        let _ = fs::remove_dir_all(path);
    }
}

/// Initialize, fetch, configure the default branch, and optionally check out files.
///
/// # Errors
///
/// Returns [`CloneError`] when the destination is invalid or fetch fails.
pub fn clone(
    opts: CloneOptions,
    progress: &mut dyn Progress,
    cancel: &dyn Cancel,
    http_factory: Option<&dyn HttpClientFactory>,
) -> std::result::Result<CloneOutcome, CloneError> {
    let dir = opts
        .dest
        .to_str()
        .ok_or_else(|| CloneError::Library(Error::Message("non-UTF-8 destination path".into())))?;
    if opts.dest.is_dir()
        && opts
            .dest
            .read_dir()
            .map(|mut entries| entries.next().is_some())
            .unwrap_or(false)
    {
        return Err(CloneError::DestNotEmpty(dir.to_owned()));
    }
    if opts.depth == Some(0) {
        return Err(CloneError::InvalidCloneDepth);
    }
    if !crate::remote::is_valid_remote_name(&opts.remote_name) {
        return Err(CloneError::Remote(RemoteError::InvalidRemoteName(
            opts.remote_name.clone(),
        )));
    }

    let bare = opts.bare;
    CloneDestinationGuard::try_claim(&opts.dest, bare)?;
    clone_after_claim(opts, progress, cancel, http_factory, bare)
}

fn clone_after_claim(
    opts: CloneOptions,
    progress: &mut dyn Progress,
    cancel: &dyn Cancel,
    http_factory: Option<&dyn HttpClientFactory>,
    bare: bool,
) -> std::result::Result<CloneOutcome, CloneError> {
    let dir = opts
        .dest
        .to_str()
        .ok_or_else(|| CloneError::Library(Error::Message("non-UTF-8 destination path".into())))?;
    let discovery_cwd = opts.environment.discovery_cwd();
    let object_format =
        local_clone_source_hash_algo(&opts.url, &discovery_cwd).unwrap_or(HashAlgo::Sha1);
    init_repository_with_object_format(
        &opts.dest,
        bare,
        &opts.initial_branch,
        None,
        RefStorageFormat::Files,
        object_format,
    )
    .map_err(|e| {
        CloneError::Library(Error::Message(format!("could not initialize '{dir}': {e}")))
    })?;
    let git_dir = if bare {
        opts.dest.clone()
    } else {
        opts.dest.join(".git")
    };
    let mut repo = open_clone_repository(
        &git_dir,
        &opts.dest,
        bare,
        &opts.environment,
        opts.promisor_fetcher.clone(),
    )
    .map_err(CloneError::Library)?;

    let origin_url = stored_clone_remote_url(&opts.url);
    set_local_config(
        &repo,
        &[(
            &format!("remote.{}.url", opts.remote_name),
            origin_url.clone(),
        )],
    )?;

    let config = ConfigSet::load(&opts.environment, Some(repo.git_dir()), true)
        .map_err(CloneError::Library)?;
    let mut remote = Remote::from_config(&config, &opts.remote_name)?;
    if bare {
        // Bare clones have no `remote.*.fetch` refspec; avoid synthesizing tracking refspecs
        // that would create `refs/remotes/<name>/…` (Git bare clone has none).
        remote.fetch_refspecs.clear();
    }
    let requested = match opts.branch.as_deref() {
        Some(name) => resolve_requested_clone_ref(name, &remote, &repo, http_factory)?,
        None => CloneRequestedRef::Default,
    };
    let fetch_refspec =
        clone_initial_fetch_refspec(&opts, &requested, &remote, &repo, http_factory)?;
    if !bare {
        let mut remote_config_entries = vec![(
            format!("remote.{}.fetch", opts.remote_name),
            fetch_refspec.clone(),
        )];
        if matches!(opts.tags, Some(CloneTags::None)) {
            remote_config_entries.push((
                format!("remote.{}.tagOpt", opts.remote_name),
                "--no-tags".to_owned(),
            ));
        }
        set_local_config(
            &repo,
            &remote_config_entries
                .iter()
                .map(|(k, v)| (k.as_str(), v.clone()))
                .collect::<Vec<_>>(),
        )?;
    }

    let clone_message = format!("clone: from {}", opts.url);
    let identity =
        fetch_operation_identity(&opts.environment, repo.git_dir()).map_err(CloneError::Library)?;
    let clone_reflog = CloneReflog {
        identity: identity.clone(),
        message: clone_message.clone(),
    };
    let mut fetch_opts = FetchOptions {
        refspecs: vec![fetch_refspec],
        tags: resolve_clone_tag_mode(&opts),
        initial_remote_fetch: true,
        remote_name: if bare {
            None
        } else {
            Some(opts.remote_name.clone())
        },
        clone_reflog: Some(clone_reflog),
        environment: opts.environment.clone(),
        thread_policy: opts.thread_policy,
        remote_progress: progress.remote_sideband_progress(),
        fetch_depth: opts.depth.and_then(crate::transfer::FetchDepth::depth),
        filter: opts.filter,
        link_source_objects: should_store_absolute_local_clone_url(&opts.url),
        ..Default::default()
    };
    if let Ok(v) = crate::protocol::try_effective_client_protocol_version_from_config(&config) {
        fetch_opts.http_protocol_version = Some(v);
    }
    let bundle = match opts.bundle_cache.as_deref() {
        Some(cache) if opts.depth.is_none() && opts.filter.is_none() => {
            remote.seed_from_bundle_uri(&repo, cache, progress, cancel, http_factory)
        }
        _ => crate::bundle_uri::BundleSeed::NotOffered,
    };
    if cancel.is_cancelled() {
        return Err(CloneError::Library(Error::Cancelled));
    }
    let mut fetch_outcome = remote.fetch(&repo, fetch_opts, progress, cancel, http_factory)?;
    if let crate::bundle_uri::BundleSeed::Seeded(seeded) = &bundle {
        crate::bundle_uri::remove_bundle_refs(&repo).map_err(CloneError::Library)?;
        fetch_outcome.transfer += seeded.transfer;
    }
    // Wire clone may upgrade an empty SHA-1 destination to the remote object format; reopen so
    // checkout uses a fresh ODB hash algorithm (the pre-fetch handle caches SHA-1).
    repo = open_clone_repository(
        &git_dir,
        &opts.dest,
        bare,
        &opts.environment,
        opts.promisor_fetcher.clone(),
    )
    .map_err(CloneError::Library)?;

    if let Some(spec) = opts.filter {
        PartialCloneConfig::write(repo.git_dir(), &opts.remote_name, &spec)
            .map_err(CloneError::Library)?;
    }

    let (head, oid) = setup_clone_head(
        &repo,
        &opts,
        &requested,
        &fetch_outcome,
        &identity,
        &clone_message,
    )?;

    let branches_with_commits = if bare {
        local_branch_names_with_commits(&repo)
    } else {
        remote_branch_names_with_commits(&repo, &opts.remote_name)
    };
    let mut checkout = classify_clone_checkout(
        oid,
        &fetch_outcome.transfer,
        fetch_outcome.branch_count(),
        &branches_with_commits,
    );

    let repo = repo.with_thread_policy(opts.thread_policy);

    let mut files = 0;
    let mut worktree_bytes = 0;
    let mut path_collisions = Vec::new();
    let mut submodule_paths = Vec::new();
    if opts.checkout && !bare {
        if let CloneCheckout::CheckedOut(checkout_oid) = checkout {
            (files, worktree_bytes, path_collisions, submodule_paths) =
                perform_clone_checkout(&repo, checkout_oid, progress, cancel)
                    .map_err(CloneError::Library)?;
        }
    } else if matches!(checkout, CloneCheckout::CheckedOut(_)) {
        checkout = CloneCheckout::Skipped;
    }

    Ok(CloneOutcome {
        url: opts.url.clone(),
        path: opts.dest.clone(),
        head,
        checkout_oid: oid,
        checkout,
        branches_with_commits,
        repo,
        transfer: fetch_outcome.transfer,
        branches: fetch_outcome.branch_count(),
        tags: fetch_outcome.tag_count(),
        files,
        worktree_bytes,
        path_collisions,
        submodule_paths,
        bare,
        bundle,
        filter_sent: fetch_outcome.filter_sent,
    })
}

fn gitlink_paths_in_index(index: &crate::index::Index) -> Vec<String> {
    index
        .entries
        .iter()
        .filter(|e| e.stage() == 0 && e.mode == crate::index::MODE_GITLINK)
        .map(|e| String::from_utf8_lossy(&e.path).into_owned())
        .collect()
}

fn perform_clone_checkout(
    repo: &Repository,
    commit: ObjectId,
    progress: &mut dyn Progress,
    cancel: &dyn Cancel,
) -> Result<(u64, u64, Vec<String>, Vec<String>)> {
    let tree = crate::diff::tree_oid_of_commit_in_repo(repo, &commit)?;
    let checkout = crate::porcelain::checkout::checkout_between_trees_with_progress(
        repo, None, &tree, progress, cancel,
    )?;
    let index = repo.index_handle().load()?;
    let ignorecase = repo
        .config()?
        .get_bool("core.ignorecase")
        .and_then(|r| r.ok())
        .unwrap_or(false);
    let (files, worktree_bytes) =
        crate::path_icase::icase_worktree_file_stats(&index, repo.work_tree(), ignorecase);
    let submodule_paths = gitlink_paths_in_index(&index);
    Ok((
        files,
        worktree_bytes,
        checkout.path_collisions,
        submodule_paths,
    ))
}

fn local_branch_names_with_commits(repo: &Repository) -> Vec<String> {
    refs::list_refs(repo.git_dir(), "refs/heads/")
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(name, oid)| {
            if oid.is_zero() {
                return None;
            }
            name.strip_prefix("refs/heads/").map(str::to_owned)
        })
        .collect()
}

fn remote_branch_names_with_commits(repo: &Repository, remote_name: &str) -> Vec<String> {
    let prefix = format!("refs/remotes/{remote_name}/");
    refs::list_refs(repo.git_dir(), &prefix)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(name, oid)| {
            if oid.is_zero() {
                return None;
            }
            name.strip_prefix(&prefix)
                .filter(|b| *b != "HEAD")
                .map(str::to_owned)
        })
        .collect()
}

fn classify_clone_checkout(
    checkout_oid: ObjectId,
    transfer: &TransferStats,
    branch_count: u64,
    branches_with_commits: &[String],
) -> CloneCheckout {
    if !checkout_oid.is_zero() {
        return CloneCheckout::CheckedOut(checkout_oid);
    }
    if transfer.objects == 0 && branch_count == 0 {
        return CloneCheckout::EmptyRemote;
    }
    if transfer.objects > 0 || !branches_with_commits.is_empty() {
        return CloneCheckout::UnbornDefaultBranch;
    }
    CloneCheckout::EmptyRemote
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CloneRequestedRef {
    Default,
    Branch(String),
    Tag { name: String, peel: ObjectId },
}

fn resolve_requested_clone_ref(
    name: &str,
    remote: &Remote,
    repo: &Repository,
    http_factory: Option<&dyn HttpClientFactory>,
) -> std::result::Result<CloneRequestedRef, CloneError> {
    let refs = remote
        .list_refs(
            Some(repo),
            &ListRefsOptions {
                symrefs: true,
                peel: true,
                ..Default::default()
            },
            http_factory,
        )
        .map_err(CloneError::Remote)?;
    let head_ref = format!("refs/heads/{name}");
    if refs.iter().any(|r| r.name == head_ref) {
        return Ok(CloneRequestedRef::Branch(name.to_owned()));
    }
    let tag_ref = format!("refs/tags/{name}");
    let folded = fold_peeled_refs(&refs);
    let Some(tag) = folded.iter().find(|r| r.name == tag_ref) else {
        return Err(CloneError::RemoteRefNotFound(name.to_owned()));
    };
    let peel = tag.peeled.filter(|oid| !oid.is_zero()).unwrap_or(tag.oid);
    if peel.is_zero() {
        return Err(CloneError::RemoteRefNotFound(name.to_owned()));
    }
    Ok(CloneRequestedRef::Tag {
        name: name.to_owned(),
        peel,
    })
}

/// Initial `remote.*.fetch` refspec and the refspec used for the first fetch.
///
/// A depth-limited clone matches `git clone --depth`: only one ref is fetched
/// (`--single-branch` behavior), not every `refs/heads/*` tip.
fn clone_initial_fetch_refspec(
    opts: &CloneOptions,
    requested: &CloneRequestedRef,
    remote: &Remote,
    repo: &Repository,
    http_factory: Option<&dyn HttpClientFactory>,
) -> std::result::Result<String, CloneError> {
    if clone_effective_single_branch(opts) {
        return match requested {
            CloneRequestedRef::Tag { name, .. } => {
                Ok(format!("+refs/tags/{name}:refs/tags/{name}"))
            }
            CloneRequestedRef::Branch(branch) => {
                if opts.bare {
                    Ok(format!("+refs/heads/{branch}:refs/heads/{branch}"))
                } else {
                    Ok(format!(
                        "+refs/heads/{branch}:refs/remotes/{}/{}",
                        opts.remote_name, branch
                    ))
                }
            }
            CloneRequestedRef::Default => {
                let branch = advertised_default_branch_from_remote(
                    remote,
                    repo,
                    http_factory,
                    &opts.url,
                    &opts.initial_branch,
                )?;
                if opts.bare {
                    Ok(format!("+refs/heads/{branch}:refs/heads/{branch}"))
                } else {
                    Ok(format!(
                        "+refs/heads/{branch}:refs/remotes/{}/{}",
                        opts.remote_name, branch
                    ))
                }
            }
        };
    }
    if opts.bare {
        return Ok("+refs/heads/*:refs/heads/*".to_owned());
    }
    Ok(format!("+refs/heads/*:refs/remotes/{}/*", opts.remote_name))
}

/// Whether the clone fetches only one branch (`git clone --single-branch` semantics).
#[must_use]
pub fn clone_effective_single_branch(opts: &CloneOptions) -> bool {
    match opts.single_branch {
        Some(true) => true,
        Some(false) => false,
        None => opts.depth.is_some_and(|d| d > 0),
    }
}

fn resolve_clone_tag_mode(opts: &CloneOptions) -> TagMode {
    opts.tags.map(CloneTags::tag_mode).unwrap_or_else(|| {
        if clone_effective_single_branch(opts) {
            TagMode::Following
        } else {
            TagMode::All
        }
    })
}

fn setup_clone_head(
    repo: &Repository,
    opts: &CloneOptions,
    requested: &CloneRequestedRef,
    fetch_outcome: &crate::transfer::FetchOutcome,
    identity: &crate::ident::Signature,
    clone_message: &str,
) -> std::result::Result<(CloneHead, ObjectId), CloneError> {
    let zero = ObjectId::zero();
    match requested {
        CloneRequestedRef::Tag { name, peel } => {
            if !peel.is_zero() {
                refs::update_direct_ref_on_store(repo.refs(), repo.git_dir(), "HEAD", peel)
                    .map_err(CloneError::Library)?;
                refs::append_reflog_with_signature(
                    repo.git_dir(),
                    "HEAD",
                    &zero,
                    peel,
                    identity,
                    clone_message,
                    false,
                )
                .map_err(CloneError::Library)?;
            }
            Ok((
                CloneHead::Detached {
                    tag: name.clone(),
                    oid: *peel,
                },
                *peel,
            ))
        }
        CloneRequestedRef::Branch(branch) => {
            let branch_ref = format!("refs/heads/{branch}");
            let oid = if opts.bare {
                refs::resolve_ref(repo.git_dir(), &branch_ref).unwrap_or(ObjectId::zero())
            } else {
                let tracking = format!("refs/remotes/{}/{}", opts.remote_name, branch);
                let oid = refs::resolve_ref(repo.git_dir(), &tracking).unwrap_or(ObjectId::zero());
                if !oid.is_zero() {
                    refs::update_direct_ref_on_store(
                        repo.refs(),
                        repo.git_dir(),
                        &branch_ref,
                        &oid,
                    )
                    .map_err(CloneError::Library)?;
                    refs::append_reflog_with_signature(
                        repo.git_dir(),
                        &branch_ref,
                        &zero,
                        &oid,
                        identity,
                        clone_message,
                        false,
                    )
                    .map_err(CloneError::Library)?;
                }
                oid
            };
            refs::write_symbolic_ref(repo.git_dir(), "HEAD", &branch_ref)
                .map_err(CloneError::Library)?;
            if !oid.is_zero() {
                refs::append_reflog_with_signature(
                    repo.git_dir(),
                    "HEAD",
                    &zero,
                    &oid,
                    identity,
                    clone_message,
                    false,
                )
                .map_err(CloneError::Library)?;
            }
            if !opts.bare {
                set_local_config(
                    repo,
                    &[
                        (&format!("branch.{branch}.remote"), opts.remote_name.clone()),
                        (
                            &format!("branch.{branch}.merge"),
                            format!("refs/heads/{branch}"),
                        ),
                    ],
                )?;
            }
            Ok((CloneHead::Branch(branch.clone()), oid))
        }
        CloneRequestedRef::Default => {
            let default = choose_clone_default_branch(
                repo,
                fetch_outcome,
                &opts.url,
                &opts.remote_name,
                &opts.initial_branch,
                opts.bare,
            );
            let branch_ref = format!("refs/heads/{default}");
            let oid = if opts.bare {
                refs::resolve_ref(repo.git_dir(), &branch_ref).unwrap_or(ObjectId::zero())
            } else {
                let tracking = format!("refs/remotes/{}/{default}", opts.remote_name);
                let oid = refs::resolve_ref(repo.git_dir(), &tracking).unwrap_or(ObjectId::zero());
                if !oid.is_zero() {
                    refs::update_direct_ref_on_store(
                        repo.refs(),
                        repo.git_dir(),
                        &branch_ref,
                        &oid,
                    )
                    .map_err(CloneError::Library)?;
                    refs::append_reflog_with_signature(
                        repo.git_dir(),
                        &branch_ref,
                        &zero,
                        &oid,
                        identity,
                        clone_message,
                        false,
                    )
                    .map_err(CloneError::Library)?;
                }
                oid
            };
            refs::write_symbolic_ref(repo.git_dir(), "HEAD", &branch_ref)
                .map_err(CloneError::Library)?;
            if !oid.is_zero() {
                refs::append_reflog_with_signature(
                    repo.git_dir(),
                    "HEAD",
                    &zero,
                    &oid,
                    identity,
                    clone_message,
                    false,
                )
                .map_err(CloneError::Library)?;
            }
            if !opts.bare {
                set_local_config(
                    repo,
                    &[
                        (
                            &format!("branch.{default}.remote"),
                            opts.remote_name.clone(),
                        ),
                        (
                            &format!("branch.{default}.merge"),
                            format!("refs/heads/{default}"),
                        ),
                    ],
                )?;
            }
            Ok((CloneHead::Branch(default), oid))
        }
    }
}

fn advertised_default_branch_from_remote(
    remote: &Remote,
    repo: &Repository,
    http_factory: Option<&dyn HttpClientFactory>,
    url: &str,
    initial_branch: &str,
) -> std::result::Result<String, CloneError> {
    let refs = remote
        .list_refs(
            Some(repo),
            &ListRefsOptions {
                symrefs: true,
                ..Default::default()
            },
            http_factory,
        )
        .map_err(CloneError::Remote)?;
    for entry in &refs {
        if entry.name == "HEAD" {
            if let Some(target) = &entry.symref_target {
                let name = target
                    .strip_prefix("refs/heads/")
                    .unwrap_or(target.as_str());
                if !name.is_empty() {
                    return Ok(name.to_owned());
                }
            }
        }
    }
    if let Some(name) = pick_default_branch_from_remote_refs(&refs) {
        return Ok(name);
    }
    if let Some(name) = default_branch_from_local_remote(url, repo) {
        return Ok(name);
    }
    Ok(initial_branch.to_owned())
}

fn pick_default_branch_from_remote_refs(refs: &[crate::remote::RemoteRef]) -> Option<String> {
    for candidate in ["main", "master"] {
        let full = format!("refs/heads/{candidate}");
        if refs.iter().any(|r| r.name == full) {
            return Some(candidate.to_owned());
        }
    }
    refs.iter()
        .find_map(|r| r.name.strip_prefix("refs/heads/").map(str::to_owned))
}

fn stored_clone_remote_url(url: &str) -> String {
    if should_store_absolute_local_clone_url(url) {
        absolute_local_clone_source_url(Path::new(url.trim()))
    } else {
        url.to_owned()
    }
}

fn set_local_config(repo: &Repository, entries: &[(&str, String)]) -> Result<()> {
    let path = repo.git_dir().join("config");
    let content = fs::read_to_string(&path).unwrap_or_default();
    let mut config = ConfigFile::parse(&path, &content, ConfigScope::Local)?;
    for (key, value) in entries {
        config.set(key, value)?;
    }
    config.write()?;
    Ok(())
}

fn choose_clone_default_branch(
    repo: &Repository,
    fetch_outcome: &crate::transfer::FetchOutcome,
    url: &str,
    remote_name: &str,
    initial_branch: &str,
    bare: bool,
) -> String {
    if let Some(advertised) = fetch_outcome.default_branch.as_deref() {
        let name = advertised.strip_prefix("refs/heads/").unwrap_or(advertised);
        if !name.is_empty() {
            return name.to_owned();
        }
    }

    let mut candidates: Vec<String> = Vec::new();
    let mut push = |name: Option<String>| {
        if let Some(name) = name {
            if !name.is_empty() && !candidates.iter().any(|c| c == &name) {
                candidates.push(name);
            }
        }
    };
    if bare {
        push(pick_default_branch_bare(repo));
    } else {
        push(pick_default_branch(repo, remote_name));
    }
    push(default_branch_from_local_remote(url, repo));
    push(Some(initial_branch.to_owned()));

    for name in &candidates {
        let resolved = if bare {
            format!("refs/heads/{name}")
        } else {
            format!("refs/remotes/{remote_name}/{name}")
        };
        if refs::resolve_ref(repo.git_dir(), &resolved).is_ok() {
            return name.clone();
        }
    }
    candidates
        .into_iter()
        .next()
        .unwrap_or_else(|| initial_branch.to_owned())
}

fn pick_default_branch_bare(repo: &Repository) -> Option<String> {
    for candidate in ["main", "master"] {
        if refs::resolve_ref(repo.git_dir(), &format!("refs/heads/{candidate}")).is_ok() {
            return Some(candidate.to_owned());
        }
    }
    refs::list_refs(repo.git_dir(), "refs/heads/")
        .ok()?
        .into_iter()
        .find_map(|(name, _)| name.strip_prefix("refs/heads/").map(str::to_owned))
}

fn open_clone_repository(
    git_dir: &Path,
    dest: &Path,
    bare: bool,
    environment: &Environment,
    promisor_fetcher: Option<Arc<dyn PromisorFetcher>>,
) -> Result<Repository> {
    if bare {
        Repository::open(
            git_dir,
            &RepositoryOptions::with_environment(environment.clone()),
        )
    } else {
        let mut open_opts = RepositoryOptions::with_environment(environment.clone())
            .with_work_tree(dest.to_path_buf());
        if let Some(fetcher) = promisor_fetcher {
            open_opts = open_opts.with_promisor_fetcher(Some(fetcher));
        }
        Repository::open(git_dir, &open_opts)
    }
}

/// When cloning from a local path, match the source repository's object hash up front.
fn local_clone_source_hash_algo(url: &str, discovery_cwd: &Path) -> Option<HashAlgo> {
    if !url_is_local_not_ssh(url.trim()) {
        return None;
    }
    let probe_git = discovery_cwd.join(".git");
    let remote_git = resolve_local_remote_git_dir(url.trim(), &probe_git, Some(discovery_cwd));
    Some(crate::transfer::open_odb(&remote_git).hash_algo())
}

fn default_branch_from_local_remote(url: &str, repo: &Repository) -> Option<String> {
    if !url_is_local_not_ssh(url.trim()) {
        return None;
    }
    let remote_git = resolve_local_remote_git_dir(url.trim(), repo.git_dir(), repo.work_tree());
    crate::gc::remote_default_branch_local(&remote_git)
        .ok()
        .flatten()
}

fn pick_default_branch(repo: &Repository, remote_name: &str) -> Option<String> {
    for candidate in ["main", "master"] {
        if refs::resolve_ref(
            repo.git_dir(),
            &format!("refs/remotes/{remote_name}/{candidate}"),
        )
        .is_ok()
        {
            return Some(candidate.to_owned());
        }
    }
    refs::list_refs(repo.git_dir(), &format!("refs/remotes/{remote_name}/"))
        .ok()?
        .into_iter()
        .find_map(|(name, _)| {
            name.strip_prefix(&format!("refs/remotes/{remote_name}/"))
                .filter(|b| *b != "HEAD")
                .map(str::to_owned)
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::progress::NeverCancel;
    use std::fs;

    #[test]
    fn dest_preexisted_empty_missing_dir() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("nope");
        assert!(!dest_preexisted_empty(&missing));
    }

    #[test]
    fn dest_preexisted_empty_empty_dir() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(dest_preexisted_empty(dir.path()));
    }

    #[test]
    fn dest_preexisted_empty_nonempty_dir() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(dir.path().join("file"), b"x").expect("write");
        assert!(!dest_preexisted_empty(dir.path()));
    }

    #[test]
    fn dest_not_empty_skips_failed_clone_cleanup() {
        assert!(!CloneError::DestNotEmpty("x".into()).should_cleanup_failed_clone());
        assert!(
            !CloneError::Remote(RemoteError::InvalidRemoteName("bad".into()))
                .should_cleanup_failed_clone()
        );
        assert!(CloneError::Library(Error::Message("x".into())).should_cleanup_failed_clone());
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn outcome_reports_transfer_stats() {
        use std::process::Command;

        let upstream = tempfile::tempdir().expect("upstream");
        let upstream_wt = upstream.path().join("src");
        std::fs::create_dir_all(&upstream_wt).expect("mkdir");
        let git = |args: &[&str]| {
            let out = Command::new("git")
                .args(args)
                .current_dir(&upstream_wt)
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
        };
        git(&["init", "-b", "main"]);
        git(&["config", "user.email", "t@example.com"]);
        git(&["config", "user.name", "Test"]);
        std::fs::write(upstream_wt.join("README.md"), b"hello\n").expect("write");
        git(&["add", "README.md"]);
        git(&["commit", "-m", "init"]);
        git(&["tag", "v1"]);
        git(&["branch", "feature"]);
        std::fs::write(upstream_wt.join("extra.txt"), b"x\n").expect("write");
        git(&["add", "extra.txt"]);
        git(&["commit", "-m", "on feature"]);
        git(&["checkout", "main"]);

        let dest = tempfile::tempdir().expect("dest");
        let outcome = clone(
            CloneOptions::new(upstream_wt.to_string_lossy(), dest.path().to_path_buf()),
            &mut crate::fetch::NoProgress,
            &NeverCancel,
            None,
        )
        .expect("clone");

        let count_out = Command::new("git")
            .args(["count-objects", "-v"])
            .current_dir(dest.path())
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .expect("count-objects");
        assert!(
            count_out.status.success(),
            "{}",
            String::from_utf8_lossy(&count_out.stderr)
        );
        let count_text = String::from_utf8_lossy(&count_out.stdout);
        let mut in_pack = None::<u64>;
        let mut loose_count = None::<u64>;
        for line in count_text.lines() {
            if let Some(v) = line.strip_prefix("in-pack: ") {
                in_pack = Some(v.trim().parse().expect("in-pack"));
            } else if let Some(v) = line.strip_prefix("count: ") {
                loose_count = Some(v.trim().parse().expect("count"));
            }
        }
        let git_objects = in_pack.expect("in-pack line") + loose_count.expect("count line");
        let git_dir = dest.path().join(".git");
        let objects_dir = git_dir.join("objects");
        let on_disk_bytes =
            |path: &Path| -> u64 { std::fs::metadata(path).map(|m| m.len()).unwrap_or(0) };
        let mut git_bytes = 0u64;
        let pack_dir = objects_dir.join("pack");
        if pack_dir.is_dir() {
            for entry in std::fs::read_dir(&pack_dir).expect("pack dir").flatten() {
                let path = entry.path();
                if path.extension().is_some_and(|x| x == "pack") {
                    git_bytes += on_disk_bytes(&path);
                }
            }
        }
        for shard in std::fs::read_dir(&objects_dir).expect("objects").flatten() {
            let name = shard.file_name().to_string_lossy().into_owned();
            if name.len() != 2 || !name.chars().all(|c| c.is_ascii_hexdigit()) {
                continue;
            }
            for obj in std::fs::read_dir(shard.path()).expect("shard").flatten() {
                git_bytes += on_disk_bytes(&obj.path());
            }
        }
        assert_eq!(
            outcome.transfer.objects, git_objects,
            "object count must match git count-objects -v (in-pack + loose)"
        );
        assert_eq!(
            outcome.transfer.bytes, git_bytes,
            "transfer bytes must match on-disk object store payload size"
        );
        assert!(outcome.branches >= 2, "expected at least main and feature");
        assert!(outcome.tags >= 1, "expected tag v1");
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn clone_unborn_advertised_trunk_not_populated_feature() {
        use std::process::Command;

        let upstream = tempfile::tempdir().expect("upstream");
        let bare = upstream.path().join("origin.git");
        let git = |args: &[&str], cwd: &std::path::Path| {
            let out = Command::new("git")
                .args(args)
                .current_dir(cwd)
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
        };
        git(
            &[
                "init",
                "-q",
                "--bare",
                "-b",
                "trunk",
                bare.to_str().unwrap(),
            ],
            upstream.path(),
        );
        git(&["symbolic-ref", "HEAD", "refs/heads/trunk"], &bare);
        let wt = upstream.path().join("wt");
        fs::create_dir_all(&wt).expect("mkdir");
        git(&["init", "-q"], &wt);
        git(&["config", "user.email", "t@example.com"], &wt);
        git(&["config", "user.name", "Test"], &wt);
        fs::write(wt.join("f"), b"x\n").expect("write");
        git(&["add", "f"], &wt);
        git(&["commit", "-m", "c"], &wt);
        git(&["branch", "feature"], &wt);
        git(&["push", bare.to_str().unwrap(), "feature"], &wt);

        let dest = tempfile::tempdir().expect("dest");
        let outcome = clone(
            CloneOptions::new(bare.to_string_lossy(), dest.path().to_path_buf()),
            &mut crate::fetch::NoProgress,
            &NeverCancel,
            None,
        )
        .expect("clone");

        assert_eq!(outcome.head.branch_name(), Some("trunk"));
        assert!(outcome.checkout_oid.is_zero());
        assert_eq!(outcome.checkout, CloneCheckout::UnbornDefaultBranch);
        assert!(
            outcome
                .branches_with_commits
                .contains(&"feature".to_owned()),
            "{:?}",
            outcome.branches_with_commits
        );
        assert!(!dest.path().join(".git/refs/heads/trunk").exists());
        assert!(!dest.path().join(".git/refs/heads/feature").exists());
        let head = fs::read_to_string(dest.path().join(".git/HEAD")).expect("HEAD");
        assert_eq!(head.trim(), "ref: refs/heads/trunk");
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn clone_empty_bare_remote_leaves_unborn_default_branch() {
        use std::process::Command;

        let upstream = tempfile::tempdir().expect("upstream");
        let bare = upstream.path().join("origin.git");
        let out = Command::new("git")
            .args(["init", "-q", "--bare", "-b", "main"])
            .arg(&bare)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .expect("git init");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );

        let dest = tempfile::tempdir().expect("dest");
        let outcome = clone(
            CloneOptions::new(bare.to_string_lossy(), dest.path().to_path_buf()),
            &mut crate::fetch::NoProgress,
            &NeverCancel,
            None,
        )
        .expect("clone empty bare");

        assert_eq!(outcome.head.branch_name(), Some("main"));
        assert!(outcome.checkout_oid.is_zero());
        assert_eq!(outcome.checkout, CloneCheckout::EmptyRemote);
        assert!(outcome.branches_with_commits.is_empty());
        assert!(!dest.path().join(".git").join("refs/heads/main").exists());
        let head = fs::read_to_string(dest.path().join(".git/HEAD")).expect("HEAD");
        assert_eq!(head.trim(), "ref: refs/heads/main");
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn classify_unborn_default_when_master_advertised_but_main_has_commits() {
        use std::process::Command;

        let upstream = tempfile::tempdir().expect("upstream");
        let bare = upstream.path().join("remote.git");
        let git = |args: &[&str], cwd: &std::path::Path| {
            let out = Command::new("git")
                .args(args)
                .current_dir(cwd)
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
        };
        git(
            &["init", "-q", "--bare", bare.to_str().unwrap()],
            upstream.path(),
        );

        let src = upstream.path().join("src");
        fs::create_dir_all(&src).expect("mkdir");
        git(&["init", "-q", "-b", "main"], &src);
        git(&["config", "user.email", "a@b.c"], &src);
        git(&["config", "user.name", "a"], &src);
        fs::write(src.join("f"), b"x\n").expect("write");
        git(&["add", "f"], &src);
        git(&["commit", "-m", "c1"], &src);
        git(&["remote", "add", "origin", bare.to_str().unwrap()], &src);
        git(&["push", "-u", "origin", "main"], &src);

        let dest = tempfile::tempdir().expect("dest");
        let outcome = clone(
            CloneOptions::new(bare.to_string_lossy(), dest.path().to_path_buf()),
            &mut crate::fetch::NoProgress,
            &NeverCancel,
            None,
        )
        .expect("clone");

        assert!(outcome.checkout_oid.is_zero());
        assert_eq!(outcome.checkout, CloneCheckout::UnbornDefaultBranch);
        assert!(
            outcome.branches_with_commits.contains(&"main".to_owned()),
            "{:?}",
            outcome.branches_with_commits
        );
        assert!(outcome.transfer.objects > 0);
    }

    #[test]
    fn derive_clone_dir_windows_style_paths() {
        assert_eq!(
            derive_clone_dir(r"..\..\origin.git", false).expect("name"),
            "origin"
        );
        assert_eq!(
            derive_clone_dir(r"C:\scratch\a\origin.git", false).expect("name"),
            "origin"
        );
        assert_eq!(
            derive_clone_dir(r"..\..\origin.git", true).expect("name"),
            "origin.git"
        );
    }

    fn git_cmd(args: &[&str], cwd: &std::path::Path) {
        use std::process::Command;
        let out = Command::new("git")
            .args(args)
            .current_dir(cwd)
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

    fn git_out(args: &[&str], cwd: &std::path::Path) -> String {
        use std::process::Command;
        let out = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .expect("git");
        assert!(out.status.success(), "git {:?} failed", args);
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn shallow_file_oids(path: &std::path::Path) -> Vec<String> {
        let shallow = path.join(".git/shallow");
        if !shallow.is_file() {
            return Vec::new();
        }
        std::fs::read_to_string(shallow)
            .expect("read shallow")
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_owned)
            .collect()
    }

    fn git_fsck_strict(work_tree: &std::path::Path) {
        use std::process::Command;
        let out = Command::new("git")
            .args(["fsck", "--strict"])
            .current_dir(work_tree)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .expect("git fsck");
        assert!(
            out.status.success(),
            "git fsck --strict: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn build_feature_upstream(bare: &std::path::Path) {
        let wt = bare.with_extension("wt");
        fs::create_dir_all(&wt).expect("mkdir");
        git_cmd(&["init", "-q", "-b", "main", "."], &wt);
        git_cmd(&["config", "user.email", "t@example.com"], &wt);
        git_cmd(&["config", "user.name", "Test"], &wt);
        for i in 1..=5 {
            fs::write(wt.join("f"), format!("{i}\n")).expect("write");
            git_cmd(&["add", "f"], &wt);
            git_cmd(&["commit", "-q", "-m", &format!("c{i}")], &wt);
        }
        git_cmd(&["branch", "feature", "HEAD~2"], &wt);
        git_cmd(&["tag", "v1", "HEAD~1"], &wt);
        let url = format!("file://{}", bare.to_string_lossy());
        git_cmd(&["clone", "-q", "--bare", ".", bare.to_str().unwrap()], &wt);
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn clone_depth_n_writes_shallow_like_git() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let bare = tmp.path().join("remote.git");
        build_feature_upstream(&bare);
        let url = format!("file://{}", bare.to_string_lossy());

        let grit_dest = tmp.path().join("grit");
        clone(
            CloneOptions {
                depth: Some(3),
                ..CloneOptions::new(url.clone(), grit_dest.clone())
            },
            &mut crate::fetch::NoProgress,
            &NeverCancel,
            None,
        )
        .expect("grit shallow clone");

        let git_dest = tmp.path().join("git");
        git_cmd(
            &[
                "clone",
                "-q",
                "--depth",
                "3",
                url.as_str(),
                git_dest.to_str().unwrap(),
            ],
            tmp.path(),
        );

        assert_eq!(
            shallow_file_oids(&grit_dest),
            shallow_file_oids(&git_dest),
            "shallow boundary set must match git"
        );
        assert_eq!(
            git_out(&["rev-list", "--count", "HEAD"], &grit_dest),
            git_out(&["rev-list", "--count", "HEAD"], &git_dest)
        );
        git_fsck_strict(&grit_dest);
        assert!(git_out(&["log", "--oneline"], &grit_dest).lines().count() >= 1);
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn clone_branch_checks_out_named_branch() {
        for depth in [None, Some(3u32)] {
            let tmp = tempfile::tempdir().expect("tempdir");
            let bare = tmp.path().join("remote.git");
            build_feature_upstream(&bare);
            let url = format!("file://{}", bare.to_string_lossy());

            let grit_dest = tmp.path().join("grit");
            clone(
                CloneOptions {
                    depth,
                    branch: Some("feature".to_owned()),
                    ..CloneOptions::new(url.clone(), grit_dest.clone())
                },
                &mut crate::fetch::NoProgress,
                &NeverCancel,
                None,
            )
            .expect("grit clone -b feature");

            let git_dest = tmp.path().join("git");
            match depth {
                None => git_cmd(
                    &[
                        "clone",
                        "-q",
                        "-b",
                        "feature",
                        url.as_str(),
                        git_dest.to_str().unwrap(),
                    ],
                    tmp.path(),
                ),
                Some(d) => {
                    let depth_s = d.to_string();
                    git_cmd(
                        &[
                            "clone",
                            "-q",
                            "--depth",
                            depth_s.as_str(),
                            "-b",
                            "feature",
                            url.as_str(),
                            git_dest.to_str().unwrap(),
                        ],
                        tmp.path(),
                    );
                }
            }

            assert_eq!(
                fs::read_to_string(grit_dest.join(".git/HEAD"))
                    .expect("HEAD")
                    .trim(),
                fs::read_to_string(git_dest.join(".git/HEAD"))
                    .expect("HEAD")
                    .trim()
            );
            assert_eq!(
                git_out(&["config", "--get", "branch.feature.remote"], &grit_dest),
                git_out(&["config", "--get", "branch.feature.remote"], &git_dest)
            );
            assert_eq!(
                git_out(&["config", "--get", "branch.feature.merge"], &grit_dest),
                git_out(&["config", "--get", "branch.feature.merge"], &git_dest)
            );
            assert_eq!(
                git_out(&["config", "--get", "remote.origin.fetch"], &grit_dest),
                git_out(&["config", "--get", "remote.origin.fetch"], &git_dest)
            );
            git_fsck_strict(&grit_dest);
        }
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn clone_tag_detaches_head() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let bare = tmp.path().join("remote.git");
        build_feature_upstream(&bare);
        let url = format!("file://{}", bare.to_string_lossy());
        let tag_commit = git_out(&["rev-parse", "v1^{}"], &bare);

        let grit_dest = tmp.path().join("grit");
        let outcome = clone(
            CloneOptions {
                branch: Some("v1".to_owned()),
                ..CloneOptions::new(url, grit_dest.clone())
            },
            &mut crate::fetch::NoProgress,
            &NeverCancel,
            None,
        )
        .expect("clone tag");

        assert!(outcome.head.is_detached());
        assert_eq!(outcome.head.tag_name(), Some("v1"));
        assert_eq!(git_out(&["rev-parse", "HEAD"], &grit_dest), tag_commit);
        let head = fs::read_to_string(grit_dest.join(".git/HEAD")).expect("HEAD");
        assert!(!head.starts_with("ref: "));
        let config = fs::read_to_string(grit_dest.join(".git/config")).expect("config");
        assert!(!config.contains("[branch "));
        git_fsck_strict(&grit_dest);
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn clone_missing_branch_is_typed_error() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let bare = tmp.path().join("remote.git");
        build_feature_upstream(&bare);
        let url = format!("file://{}", bare.to_string_lossy());
        let dest = tmp.path().join("dest");
        let err = clone(
            CloneOptions {
                branch: Some("no-such-ref".to_owned()),
                ..CloneOptions::new(url, dest.clone())
            },
            &mut crate::fetch::NoProgress,
            &NeverCancel,
            None,
        )
        .expect_err("missing ref");
        assert!(matches!(err, CloneError::RemoteRefNotFound(_)));
        assert!(err.should_cleanup_failed_clone());
        cleanup_failed_clone(&dest, false);
        assert!(!dest.exists());
    }

    #[test]
    fn clone_rejects_nonempty_destination_without_git_dir() {
        let upstream = tempfile::tempdir().expect("upstream");
        let dest = tempfile::tempdir().expect("dest");
        fs::write(dest.path().join("notes.txt"), b"keep").expect("write");
        let err = clone(
            CloneOptions::new(upstream.path().to_string_lossy(), dest.path().to_path_buf()),
            &mut crate::fetch::NoProgress,
            &NeverCancel,
            None,
        )
        .expect_err("non-empty dest");
        assert!(matches!(err, CloneError::DestNotEmpty(_)));
        assert!(dest.path().join("notes.txt").is_file());
        assert!(!dest.path().join(".git").exists());
    }
}
