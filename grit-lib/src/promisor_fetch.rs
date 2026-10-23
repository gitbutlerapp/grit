//! On-demand fetch of objects promised by a partial-clone promisor remote.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use crate::config::ConfigSet;
use crate::environment::Environment;
use crate::error::{Error, Result};
use crate::fetch::NoProgress;
use crate::objects::pack::clear_pack_cache;
use crate::objects::ObjectId;
use crate::partial_clone::{remote_is_promisor, stored_remote_filter};
use crate::promisor::repo_treats_promisor_packs;
use crate::remote::{HttpClientFactory, Remote, DEFAULT_REMOTE};
use crate::repo::Repository;
use crate::transfer::{FetchOptions, TagMode};

/// Environment variable that disables promisor lazy fetch (same semantics as Git's `GIT_NO_LAZY_FETCH`).
pub const NO_LAZY_FETCH_ENV: &str = "GRIT_NO_LAZY_FETCH";

/// Config key to disable automatic promisor lazy fetch (`true` by default when unset).
pub const PROMISOR_LAZY_FETCH_CONFIG: &str = "fetch.promisorLazy";

/// Maximum nested lazy-fetch depth (matches Git's promisor-remote guard).
const MAX_LAZY_FETCH_DEPTH: u32 = 5;

/// Fetches missing objects from the configured promisor remote without updating refs.
pub trait PromisorFetcher: Send + Sync {
    /// Fetch every listed object into the repository (typically one promisor pack).
    ///
    /// # Errors
    ///
    /// Propagates transport, negotiation, and pack ingest failures.
    fn fetch_missing(&self, repo: &Repository, oids: &[ObjectId]) -> Result<()>;
}

/// Default [`PromisorFetcher`] using [`Remote::fetch`] and the process HTTP client factory.
pub struct DefaultPromisorFetcher {
    http_factory: Arc<dyn HttpClientFactory>,
}

impl DefaultPromisorFetcher {
    /// Build a fetcher that uses `factory` for HTTP(S) promisor remotes.
    #[must_use]
    pub fn new(http_factory: Arc<dyn HttpClientFactory>) -> Self {
        Self { http_factory }
    }
}

impl PromisorFetcher for DefaultPromisorFetcher {
    fn fetch_missing(&self, repo: &Repository, oids: &[ObjectId]) -> Result<()> {
        fetch_promisor_objects(repo, oids, Some(&*self.http_factory))
    }
}

/// Whether automatic promisor lazy fetch is enabled for this repository.
#[must_use]
pub fn promisor_lazy_fetch_enabled(config: &ConfigSet, env: &Environment) -> bool {
    if lazy_fetch_disabled_by_env(env) {
        return false;
    }
    match config.get_bool(PROMISOR_LAZY_FETCH_CONFIG) {
        Some(Ok(false)) => false,
        Some(Err(_)) | None | Some(Ok(true)) => true,
    }
}

fn lazy_fetch_disabled_by_env(env: &Environment) -> bool {
    for key in [NO_LAZY_FETCH_ENV, "GIT_NO_LAZY_FETCH"] {
        if let Some(v) = env.var(key) {
            if v == "1" || v.eq_ignore_ascii_case("true") {
                return true;
            }
        }
    }
    false
}

fn partial_clone_remote_name(config: &ConfigSet) -> Option<String> {
    config.get("extensions.partialclone").map(|s| s.to_owned())
}

/// Batch-fetch missing object ids from the promisor remote (no ref updates).
///
/// # Errors
///
/// Returns [`Error::Message`] when no promisor remote is configured, or transport failures.
pub fn fetch_promisor_objects(
    repo: &Repository,
    oids: &[ObjectId],
    http_factory: Option<&dyn HttpClientFactory>,
) -> Result<()> {
    let unique = dedupe_oids(oids);
    if unique.is_empty() {
        return Ok(());
    }
    let missing: Vec<ObjectId> = unique
        .into_iter()
        .filter(|oid| !repo.odb().exists_local(oid))
        .collect();
    if missing.is_empty() {
        return Ok(());
    }

    let config = repo.config()?;
    let remote_name = partial_clone_remote_name(config.as_ref())
        .or_else(|| Some(DEFAULT_REMOTE.to_owned()))
        .filter(|name| remote_is_promisor(config.as_ref(), name))
        .ok_or_else(|| Error::Message("no promisor remote configured for partial clone".into()))?;

    let remote = Remote::from_config(config.as_ref(), &remote_name)?;
    let mut fetch_opts = FetchOptions {
        tags: TagMode::None,
        remote_name: Some(remote_name.clone()),
        environment: repo.environment().clone(),
        thread_policy: repo.thread_policy(),
        network_trace: repo.network_trace_enabled(),
        diagnostics: Some(repo.diagnostics()),
        filter: stored_remote_filter(config.as_ref(), &remote_name),
        object_wants: missing,
        promisor_object_fetch: true,
        ..Default::default()
    };
    if let Ok(v) =
        crate::protocol::try_effective_client_protocol_version_from_config(config.as_ref())
    {
        fetch_opts.http_protocol_version = Some(v);
    }

    let mut progress = NoProgress;
    remote.fetch(repo, fetch_opts, &mut progress, http_factory)?;
    clear_pack_cache();
    repo.odb().invalidate_packs();
    Ok(())
}

fn dedupe_oids(oids: &[ObjectId]) -> Vec<ObjectId> {
    let mut out = Vec::new();
    for oid in oids {
        if !out.contains(oid) {
            out.push(*oid);
        }
    }
    out
}

/// Lazy-fetch depth guard stored on [`crate::repo_caches::RepoCaches`].
#[derive(Default)]
pub(crate) struct PromisorLazyFetchDepth(AtomicU32);

impl PromisorLazyFetchDepth {
    pub(crate) fn enter(&self) -> Result<PromisorLazyFetchGuard<'_>> {
        let prev = self.0.fetch_add(1, Ordering::SeqCst);
        if prev >= MAX_LAZY_FETCH_DEPTH {
            self.0.fetch_sub(1, Ordering::SeqCst);
            return Err(Error::Message(format!(
                "too many nested promisor lazy fetches (limit {MAX_LAZY_FETCH_DEPTH})"
            )));
        }
        Ok(PromisorLazyFetchGuard { slot: &self.0 })
    }
}

pub(crate) struct PromisorLazyFetchGuard<'a> {
    slot: &'a AtomicU32,
}

impl Drop for PromisorLazyFetchGuard<'_> {
    fn drop(&mut self) {
        self.slot.fetch_sub(1, Ordering::SeqCst);
    }
}

/// ODB read miss hook: lazy-fetch or surface [`Error::PromisedObjectMissing`].
pub(crate) fn handle_promisor_read_miss(
    repo_caches: &crate::repo_caches::RepoCaches,
    odb: &crate::odb::Odb,
    oid: &ObjectId,
    env: &Environment,
) -> Result<Option<()>> {
    let git_dir = odb.config_git_dir().ok_or_else(|| {
        Error::Message("promisor lazy fetch requires a git directory on the ODB".into())
    })?;
    let config = repo_caches.load_config(env, Some(git_dir), true)?;
    if !repo_treats_promisor_packs(git_dir, config.as_ref()) {
        return Ok(None);
    }
    if !promisor_lazy_fetch_enabled(config.as_ref(), env) {
        return Err(Error::PromisedObjectMissing(*oid));
    }
    let Some(fetcher) = repo_caches.promisor_fetcher() else {
        return Err(Error::PromisedObjectMissing(*oid));
    };
    let _depth = repo_caches.promisor_lazy_depth().enter()?;
    let repo = open_repo_for_promisor_fetch(git_dir, env, repo_caches)?;
    fetcher.fetch_missing(&repo, &[*oid])?;
    odb.reprepare_local_packs();
    odb.invalidate_packs();
    Ok(Some(()))
}

fn open_repo_for_promisor_fetch(
    git_dir: &std::path::Path,
    env: &Environment,
    caches: &crate::repo_caches::RepoCaches,
) -> Result<Repository> {
    use crate::environment::RepositoryOptions;
    let mut opts = RepositoryOptions::with_environment(env.clone());
    opts.diagnostics = caches.diagnostics_handle();
    opts.network_trace = false;
    let work_tree = git_dir
        .file_name()
        .and_then(|n| n.to_str())
        .filter(|&n| n == ".git")
        .and_then(|_| git_dir.parent())
        .map(std::path::Path::to_path_buf);
    let opts = if let Some(wt) = work_tree {
        opts.with_work_tree(wt)
    } else {
        opts
    };
    Repository::open(git_dir, &opts.without_format_validation())
}
