//! On-demand fetch of objects promised by a partial-clone promisor remote.

use std::cell::Cell;
use std::sync::Arc;

use crate::config::ConfigSet;
use crate::environment::Environment;
use crate::error::{Error, Result};
use crate::fetch::NoProgress;
use crate::objects::pack::clear_pack_cache;
use crate::objects::ObjectId;
use crate::objects::OidHashSet;
use crate::partial_clone::ObjectFilterSpec;
use crate::promisor::repo_treats_promisor_packs;
use crate::promisor_remote::lazy_fetch_order;
use crate::remote::{HttpClientFactory, Remote};
use crate::repo::Repository;
use crate::transfer::{FetchOptions, TagMode};

/// Environment variable that disables promisor lazy fetch (same semantics as Git's `GIT_NO_LAZY_FETCH`).
pub const NO_LAZY_FETCH_ENV: &str = "GRIT_NO_LAZY_FETCH";

/// Config key to disable automatic promisor lazy fetch (`true` by default when unset).
pub const PROMISOR_LAZY_FETCH_CONFIG: &str = "fetch.promisorLazy";

/// Maximum lazy-fetch retries for one read when the object stays missing after fetch.
const MAX_LAZY_READ_RETRIES: u32 = 5;

/// Maximum nested promisor lazy-fetch depth on one thread (matches Git's promisor-remote guard).
const MAX_LAZY_FETCH_NEST: u32 = 5;

thread_local! { // hygiene: per-thread promisor lazy-fetch nesting depth (Git parity guard)
    static PROMISOR_LAZY_NEST: Cell<u32> = const { Cell::new(0) };
}

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

/// Batch-fetch missing object ids from the promisor remotes (no ref updates).
///
/// Promisor remotes are tried one after another in Git's order (see
/// [`lazy_fetch_order`]): config order, with the `extensions.partialClone` remote
/// last, so a large-object promisor configured next to `origin` is asked first.
/// Each remote is asked only for the objects still missing, with
/// `filter blob:none` as Git sends it, using the URL configured locally.
///
/// A server refuses a whole request when it lacks one of the wants, so when a remote
/// fails and several objects are still missing, the rest is split in halves and each
/// half is tried on every remote again: a large-object promisor holding only the large
/// blobs and `origin` holding the small ones then serve one checkout between them. A
/// remote that cannot be used at all (unreachable, no URL, authentication failed) is
/// not asked again.
///
/// # Errors
///
/// Returns [`Error::Message`] when no promisor remote is configured,
/// [`Error::PromisorFetchFailed`] (each failed remote with its first error) when
/// objects are still missing and some remote failed, [`Error::PromisedObjectMissing`]
/// when every remote answered but none had the object, and [`Error::Cancelled`] when a
/// fetch was cancelled.
pub fn fetch_promisor_objects(
    repo: &Repository,
    oids: &[ObjectId],
    http_factory: Option<&dyn HttpClientFactory>,
) -> Result<()> {
    let unique = dedupe_oids(oids);
    if unique.is_empty() {
        return Ok(());
    }
    let missing = repo.odb().missing_stored_oids(unique)?;
    if missing.is_empty() {
        return Ok(());
    }

    let config = repo.config()?;
    let order = lazy_fetch_order(config.as_ref(), &[]);
    if order.is_empty() {
        return Err(Error::Message(
            "no promisor remote configured for partial clone".into(),
        ));
    }
    let protocol =
        crate::protocol::try_effective_client_protocol_version_from_config(config.as_ref()).ok();

    let _nest = PromisorLazyNestGuard::enter()?;
    fetch_from_promisors(repo, &missing, &order, &mut |remote_name, wants| {
        let remote = Remote::from_config(config.as_ref(), remote_name)
            .map_err(|e| PromisorAttemptError::RemoteUnusable(e.into()))?;
        let fetch_opts = FetchOptions {
            tags: TagMode::None,
            remote_name: Some(remote_name.to_owned()),
            environment: repo.environment().clone(),
            thread_policy: repo.thread_policy(),
            network_trace: repo.network_trace_enabled(),
            diagnostics: Some(repo.diagnostics()),
            filter: Some(ObjectFilterSpec::BlobNone),
            object_wants: wants.to_vec(),
            promisor_object_fetch: true,
            http_protocol_version: protocol,
            ..Default::default()
        };
        let mut progress = NoProgress;
        remote
            .fetch(
                repo,
                fetch_opts,
                &mut progress,
                &crate::progress::NeverCancel,
                http_factory,
            )
            .map(|_| ())
            .map_err(PromisorAttemptError::from)
    })
}

/// How one lazy-fetch request to one promisor remote failed.
pub(crate) enum PromisorAttemptError {
    /// The remote cannot be used at all (unreachable, no usable URL, authentication
    /// failed), so it is not asked again.
    RemoteUnusable(Error),
    /// This request failed, perhaps because the remote lacks one of the objects; a
    /// smaller request may still work.
    RequestFailed(Error),
}

impl From<crate::remote::RemoteError> for PromisorAttemptError {
    fn from(e: crate::remote::RemoteError) -> Self {
        match e {
            crate::remote::RemoteError::Library(err)
                if err.is_remote_unreadable() || matches!(err, Error::Auth { .. }) =>
            {
                Self::RemoteUnusable(err)
            }
            crate::remote::RemoteError::Library(err) => Self::RequestFailed(err),
            other => Self::RemoteUnusable(other.into()),
        }
    }
}

/// One lazy-fetch request: fetch these object ids from the named remote.
pub(crate) type FetchOne<'a> =
    dyn FnMut(&str, &[ObjectId]) -> std::result::Result<(), PromisorAttemptError> + 'a;

/// Ask the remotes in `order` for the objects of `oids`, until none is missing.
///
/// `fetch_one(remote, wants)` fetches `wants` from `remote`. Each batch (at first every
/// object) is offered to each usable remote in turn, for what is still missing; after
/// every attempt the pack list is refreshed and the missing set recomputed. When a
/// remote failed on the batch and more than one object is left, the rest is split in
/// two halves that go through the remotes again, so remotes that each hold some of
/// the objects can serve them between them.
///
/// # Errors
///
/// When objects are still missing at the end: [`Error::PromisorFetchFailed`] when some
/// attempt failed, or [`Error::PromisedObjectMissing`] when every attempt succeeded.
/// A cancelled attempt ([`Error::is_cancelled`]) stops at once with that error.
pub(crate) fn fetch_from_promisors(
    repo: &Repository,
    oids: &[ObjectId],
    order: &[String],
    fetch_one: &mut FetchOne<'_>,
) -> Result<()> {
    let mut failures: Vec<(String, Error)> = Vec::new();
    let mut unusable: Vec<&str> = Vec::new();
    let mut unresolved: Vec<ObjectId> = Vec::new();
    let mut batches: Vec<Vec<ObjectId>> = vec![oids.to_vec()];
    while let Some(mut missing) = batches.pop() {
        let mut failed = false;
        for name in order {
            if missing.is_empty() {
                break;
            }
            if unusable.contains(&name.as_str()) {
                continue;
            }
            let err = match fetch_one(name, &missing) {
                Ok(()) => None,
                Err(PromisorAttemptError::RemoteUnusable(err)) => {
                    unusable.push(name);
                    Some(err)
                }
                Err(PromisorAttemptError::RequestFailed(err)) => Some(err),
            };
            if let Some(err) = err {
                if err.is_cancelled() {
                    return Err(err);
                }
                failed = true;
                if !failures.iter().any(|(n, _)| n == name) {
                    failures.push((name.clone(), err));
                }
            }
            clear_pack_cache();
            repo.odb().reprepare_local_packs();
            repo.odb().invalidate_packs();
            missing = repo.odb().missing_stored_oids(missing)?;
        }
        if failed && missing.len() > 1 && unusable.len() < order.len() {
            let second = missing.split_off(missing.len() / 2);
            batches.push(second);
            batches.push(missing);
        } else {
            unresolved.extend(missing);
        }
    }
    match unresolved.first() {
        None => Ok(()),
        Some(&first) if failures.is_empty() => Err(Error::PromisedObjectMissing(first)),
        Some(&first) => Err(Error::PromisorFetchFailed {
            missing: first,
            failures,
        }),
    }
}

fn dedupe_oids(oids: &[ObjectId]) -> Vec<ObjectId> {
    let mut set = OidHashSet::default();
    for oid in oids {
        set.insert(*oid);
    }
    set.into_iter().collect()
}

struct PromisorLazyNestGuard;

impl PromisorLazyNestGuard {
    fn enter() -> Result<Self> {
        PROMISOR_LAZY_NEST.with(|cell| {
            let prev = cell.get();
            if prev >= MAX_LAZY_FETCH_NEST {
                return Err(Error::Message(format!(
                    "too many nested promisor lazy fetches (limit {MAX_LAZY_FETCH_NEST})"
                )));
            }
            cell.set(prev + 1);
            Ok(Self)
        })
    }
}

impl Drop for PromisorLazyNestGuard {
    fn drop(&mut self) {
        PROMISOR_LAZY_NEST.with(|cell| cell.set(cell.get().saturating_sub(1)));
    }
}

/// Outcome of attempting promisor lazy fetch after an ODB read miss.
pub(crate) enum PromisorMissResolution {
    /// Object should be present; caller retries the read.
    RetryAfterFetch,
    /// Not a partial-clone repository — surface [`Error::ObjectNotFound`].
    NotPromisorRepo,
    /// Lazy fetch disabled or no fetcher installed.
    PromisedMissing(Error),
}

/// Batch-fetch missing promisor objects before many reads (checkout, tree diffs).
///
/// No-ops when the repository is not promisor-backed, lazy fetch is disabled, no fetcher
/// is installed, or [`Odb`] has no shared repository config snapshot.
pub(crate) fn prefetch_promisor_read_batch(
    repo_caches: &crate::repo::RepositoryConfigSnapshot,
    odb: &crate::odb::Odb,
    env: &Environment,
    oids: impl IntoIterator<Item = ObjectId>,
) -> Result<()> {
    let Some(git_dir) = odb.config_git_dir() else {
        return Ok(());
    };
    let config = repo_caches.load_config(env, Some(git_dir), true)?;
    if !repo_treats_promisor_packs(git_dir, config.as_ref())
        || !promisor_lazy_fetch_enabled(config.as_ref(), env)
    {
        return Ok(());
    }
    let Some(fetcher) = repo_caches.promisor_fetcher() else {
        return Ok(());
    };
    let unique: OidHashSet = oids.into_iter().filter(|oid| !oid.is_zero()).collect();
    if unique.is_empty() {
        return Ok(());
    }
    let missing = odb.missing_stored_oids(unique)?;
    if missing.is_empty() {
        return Ok(());
    }

    let _nest = PromisorLazyNestGuard::enter()?;
    let repo = open_repo_for_promisor_fetch(git_dir, env, repo_caches)?;
    fetcher.fetch_missing(&repo, &missing)?;
    odb.reprepare_local_packs();
    odb.invalidate_packs();
    Ok(())
}

/// After an ODB read miss, optionally fetch from the promisor remote and signal retry.
pub(crate) fn resolve_promisor_read_miss(
    repo_caches: &crate::repo_caches::RepoCaches,
    odb: &crate::odb::Odb,
    oid: &ObjectId,
    env: &Environment,
    lazy_retry_count: u32,
) -> Result<PromisorMissResolution> {
    if lazy_retry_count >= MAX_LAZY_READ_RETRIES {
        return Err(Error::Message(format!(
            "promisor lazy fetch did not materialize {} after {MAX_LAZY_READ_RETRIES} attempts",
            oid.to_hex()
        )));
    }

    let git_dir = odb.config_git_dir().ok_or_else(|| {
        Error::Message("promisor lazy fetch requires a git directory on the ODB".into())
    })?;
    let config = repo_caches.load_config(env, Some(git_dir), true)?;
    if !repo_treats_promisor_packs(git_dir, config.as_ref()) {
        return Ok(PromisorMissResolution::NotPromisorRepo);
    }
    if !promisor_lazy_fetch_enabled(config.as_ref(), env) {
        return Ok(PromisorMissResolution::PromisedMissing(
            Error::PromisedObjectMissing(*oid),
        ));
    }
    let Some(fetcher) = repo_caches.promisor_fetcher() else {
        return Ok(PromisorMissResolution::PromisedMissing(
            Error::PromisedObjectMissing(*oid),
        ));
    };

    let _nest = PromisorLazyNestGuard::enter()?;
    let repo = open_repo_for_promisor_fetch(git_dir, env, repo_caches)?;
    fetcher.fetch_missing(&repo, &[*oid])?;
    odb.reprepare_local_packs();
    odb.invalidate_packs();

    if !odb.try_exists_stored(oid)? {
        return Err(Error::Message(format!(
            "promisor fetch completed but {} is still missing locally",
            oid.to_hex()
        )));
    }
    Ok(PromisorMissResolution::RetryAfterFetch)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::objects::ObjectKind;
    use crate::repo::InitOptions;

    fn repo() -> (tempfile::TempDir, Repository) {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path(), &InitOptions::new().with_bare(true)).unwrap();
        (dir, repo)
    }

    fn blob_id(repo: &Repository, body: &[u8]) -> ObjectId {
        repo.odb()
            .hash_algo()
            .hash_object(ObjectKind::Blob, body)
            .unwrap()
    }

    fn order(names: &[&str]) -> Vec<String> {
        names.iter().map(|n| (*n).to_owned()).collect()
    }

    fn failed(msg: &str) -> PromisorAttemptError {
        PromisorAttemptError::RequestFailed(Error::Message(msg.to_owned()))
    }

    /// A remote holding `bodies`: like a server, it refuses the whole request when it
    /// lacks any want, and otherwise writes every want.
    fn serve(
        repo: &Repository,
        bodies: &[&[u8]],
        wants: &[ObjectId],
    ) -> std::result::Result<(), PromisorAttemptError> {
        let held: Vec<(ObjectId, &[u8])> = bodies.iter().map(|b| (blob_id(repo, b), *b)).collect();
        if let Some(lacking) = wants.iter().find(|w| !held.iter().any(|(id, _)| id == *w)) {
            return Err(failed(&format!("not our ref {lacking}")));
        }
        for (_, body) in held.iter().filter(|(id, _)| wants.contains(id)) {
            repo.odb()
                .write_loose_materialize(ObjectKind::Blob, body)
                .map_err(PromisorAttemptError::RequestFailed)?;
        }
        Ok(())
    }

    #[test]
    fn asks_remotes_in_order_for_what_is_still_missing() {
        let (_dir, repo) = repo();
        let a = blob_id(&repo, b"on the lop\n");
        let b = blob_id(&repo, b"only on origin\n");
        let mut calls: Vec<(String, Vec<ObjectId>)> = Vec::new();
        fetch_from_promisors(
            &repo,
            &[a, b],
            &order(&["lop", "origin"]),
            &mut |name, wants| {
                calls.push((name.to_owned(), wants.to_vec()));
                let body: &[u8] = if name == "lop" {
                    b"on the lop\n"
                } else {
                    b"only on origin\n"
                };
                repo.odb()
                    .write_loose_materialize(ObjectKind::Blob, body)
                    .map_err(PromisorAttemptError::RequestFailed)?;
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(
            calls,
            [
                ("lop".to_owned(), vec![a, b]),
                ("origin".to_owned(), vec![b])
            ]
        );
    }

    #[test]
    fn stops_once_nothing_is_missing() {
        let (_dir, repo) = repo();
        let a = blob_id(&repo, b"x\n");
        let mut asked = Vec::new();
        fetch_from_promisors(&repo, &[a], &order(&["lop", "origin"]), &mut |name, w| {
            asked.push(name.to_owned());
            serve(&repo, &[b"x\n"], w)
        })
        .unwrap();
        assert_eq!(asked, ["lop"]);
    }

    #[test]
    fn falls_back_after_a_failing_remote() {
        let (_dir, repo) = repo();
        let a = blob_id(&repo, b"x\n");
        fetch_from_promisors(&repo, &[a], &order(&["lop", "origin"]), &mut |name, w| {
            if name == "lop" {
                return Err(failed("lop is down"));
            }
            serve(&repo, &[b"x\n"], w)
        })
        .unwrap();
    }

    #[test]
    fn remotes_that_each_hold_some_objects_serve_them_between_them() {
        // The LOP holds the large blobs, origin the small ones; each refuses a request
        // that names anything it lacks, as a server does.
        let (_dir, repo) = repo();
        let lop: [&[u8]; 2] = [b"big one\n", b"big two\n"];
        let origin: [&[u8]; 5] = [b"s1\n", b"s2\n", b"s3\n", b"s4\n", b"s5\n"];
        let wanted: Vec<ObjectId> = lop
            .iter()
            .chain(&origin)
            .map(|b| blob_id(&repo, b))
            .collect();
        let mut requests = 0;
        fetch_from_promisors(
            &repo,
            &wanted,
            &order(&["lop", "origin"]),
            &mut |name, w| {
                requests += 1;
                serve(&repo, if name == "lop" { &lop } else { &origin }, w)
            },
        )
        .unwrap();
        assert!(repo.odb().missing_stored_oids(wanted).unwrap().is_empty());
        assert!(requests < 20, "{requests} requests");
    }

    #[test]
    fn an_unusable_remote_is_not_asked_again() {
        let (_dir, repo) = repo();
        let wanted: Vec<ObjectId> = [b"a\n", b"b\n", b"c\n"]
            .iter()
            .map(|b| blob_id(&repo, *b))
            .collect();
        let mut lop_asked = 0;
        let err = fetch_from_promisors(
            &repo,
            &wanted,
            &order(&["lop", "origin"]),
            &mut |name, w| {
                if name == "lop" {
                    lop_asked += 1;
                    return Err(PromisorAttemptError::RemoteUnusable(Error::Message(
                        "bad lop url".into(),
                    )));
                }
                serve(&repo, &[b"a\n"], w)
            },
        )
        .unwrap_err();
        assert_eq!(lop_asked, 1);
        // Origin still served the one object it holds.
        assert!(repo
            .odb()
            .missing_stored_oids(vec![wanted[0]])
            .unwrap()
            .is_empty());
        let Error::PromisorFetchFailed { failures, .. } = err else {
            panic!("expected PromisorFetchFailed, got {err:?}");
        };
        let names: Vec<&str> = failures.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["lop", "origin"]);
        assert!(failures[0].1.to_string().contains("bad lop url"));
    }

    #[test]
    fn reports_every_remote_error_or_the_missing_object() {
        let (_dir, repo) = repo();
        let a = blob_id(&repo, b"x\n");
        let err = fetch_from_promisors(&repo, &[a], &order(&["lop", "origin"]), &mut |name, _| {
            Err(failed(&format!("{name} failed")))
        })
        .unwrap_err();
        let shown = err.to_string();
        assert!(shown.contains("lop: lop failed"), "{shown}");
        assert!(shown.contains("origin: origin failed"), "{shown}");
        assert!(
            matches!(err, Error::PromisorFetchFailed { missing, ref failures } if missing == a && failures.len() == 2)
        );

        let err =
            fetch_from_promisors(&repo, &[a], &order(&["lop"]), &mut |_, _| Ok(())).unwrap_err();
        assert!(matches!(err, Error::PromisedObjectMissing(oid) if oid == a));
    }

    #[test]
    fn a_cancelled_fetch_stops_at_once() {
        let (_dir, repo) = repo();
        let a = blob_id(&repo, b"x\n");
        let mut asked = 0;
        let err = fetch_from_promisors(&repo, &[a], &order(&["lop", "origin"]), &mut |_, _| {
            asked += 1;
            Err(PromisorAttemptError::RequestFailed(Error::Cancelled))
        })
        .unwrap_err();
        assert!(err.is_cancelled());
        assert_eq!(asked, 1);
    }

    #[test]
    fn no_promisor_remote_is_an_error() {
        let (_dir, repo) = repo();
        let a = blob_id(&repo, b"x\n");
        let err = fetch_promisor_objects(&repo, &[a], None).unwrap_err();
        assert!(err.to_string().contains("no promisor remote"), "{err}");
    }
}
