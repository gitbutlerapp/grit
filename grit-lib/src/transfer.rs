//! Embedder-facing transfer (fetch / push) result & option types, plus the
//! negotiation-driven pack builder.
//!
//! This module is the foundation for the in-process fetch/push APIs that
//! embedders such as `jj` and GitButler consume in place of `gix` transport.
//! It defines the structured input/output types those APIs use and implements
//! the single most important primitive — [`build_pack`] — which packs **only**
//! the objects reachable from a negotiated set of `wants` and not already
//! reachable from the remote's `haves`.
//!
//! Scope note (phase 1): only the local / `file://` object+ref copy path is in
//! scope. `git://`, `http(s)`, and `ssh` transports plus credential-helper
//! execution are out of scope and are left as TODOs in later phases.
//!
//! Push *result* reporting reuses [`crate::push_report::PushRefResult`] /
//! [`crate::push_report::PushRefStatus`] rather than redefining it.

use crate::environment::RepositoryOptions;

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::fetch::{NoProgress, Progress};
use crate::hash;
use crate::objects::{parse_commit, parse_tag, parse_tree, HashAlgo, ObjectId, ObjectKind};
use crate::odb::Odb;
use crate::progress::{ProgressPhase, ProgressSink};
use crate::push_report::{PushRefResult, PushRefStatus};
use crate::refspec::{parse_fetch_refspec, RefspecItem};

/// How a single reference resolved during a fetch (or would resolve in a push).
///
/// Mirrors the shapes of `gix::remote::fetch::refs::update::Mode` that `jj`
/// already consumes, so the embedder's translation layer stays a thin adapter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UpdateMode {
    /// The local tracking ref did not exist and was created.
    New,
    /// The update advanced the ref along its existing history.
    FastForward,
    /// A non-fast-forward update that was applied because force was requested.
    Forced,
    /// The local ref already matched the remote value; nothing to do.
    UpToDate,
    /// No change was required (e.g. a no-op refspec).
    NoChangeNeeded,
    /// A non-fast-forward update that was rejected (force not requested).
    NonFastForwardRejected,
    /// A tag update was rejected (tags are not overwritten without force).
    TagUpdateRejected,
    /// The source object named by the refspec was not found on the remote.
    SourceObjectNotFound,
    /// The remote ref is unborn (points at nothing yet).
    Unborn,
    /// A prune/delete was requested but the local ref was already missing.
    DeletedMissing,
}

impl UpdateMode {
    /// Returns whether fetch applied this update to the local repository.
    ///
    /// Rejected or no-op outcomes (for example an existing tag that moved on the
    /// remote) are excluded so callers can report only refs that actually changed.
    #[must_use]
    pub fn modified_repo(self) -> bool {
        matches!(
            self,
            Self::New | Self::FastForward | Self::Forced | Self::DeletedMissing
        )
    }
}

/// The resolved outcome of one reference during a fetch.
#[derive(Clone, Debug)]
pub struct RefUpdate {
    /// The remote-side ref name (e.g. `refs/heads/main`).
    pub remote_ref: String,
    /// The local-side ref name written, if any (e.g. `refs/remotes/origin/main`).
    pub local_ref: Option<String>,
    /// Previous value of the local ref (`None` when newly created).
    pub old_oid: Option<ObjectId>,
    /// New value written to the local ref (`None` for deletions / unborn).
    pub new_oid: Option<ObjectId>,
    /// How the update resolved.
    pub mode: UpdateMode,
    /// Optional human-readable note (reason text), for embedder display.
    pub note: Option<String>,
}

/// Which tags to fetch alongside the requested refs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum TagMode {
    /// Do not fetch any tags automatically.
    None,
    /// Fetch tags that point at commits reachable from the refs being fetched
    /// (same idea as `git fetch --follow-tags`; not what `git clone` does).
    ///
    /// Tags the local repository already has at the advertised value are not
    /// considered again, so they do not appear in [`FetchOutcome::updates`].
    #[default]
    Following,
    /// Fetch every advertised `refs/tags/*` ref (same idea as `git clone` and
    /// `git fetch --tags`).
    All,
}

/// Options controlling a fetch.
pub struct FetchOptions {
    /// Positive refspecs selecting what to fetch.
    pub refspecs: Vec<String>,
    /// Negative refspecs excluding refs from the positive set.
    pub negative_refspecs: Vec<String>,
    /// Tag-following policy.
    pub tags: TagMode,
    /// Whether to prune local tracking refs that vanished on the remote.
    pub prune: bool,
    /// Compute and report updates without writing any refs or objects.
    pub dry_run: bool,
    /// Truncate history to the given number of commits per tip
    /// (`git fetch --depth N`). Drives the wire `deepen N` / v2 `deepen` arg and,
    /// for a previously shallow repo, deepens the existing boundary. `None`
    /// requests full history.
    pub depth: Option<u32>,
    /// Deepen history to include commits no older than this cutoff
    /// (`git fetch --shallow-since <date>`). The value is sent verbatim as the
    /// wire `deepen-since <value>`; callers should pass the Unix timestamp Git's
    /// `upload-pack` expects (a bare integer), not a human date string.
    pub deepen_since: Option<String>,
    /// Deepen history but stop at (exclude) commits reachable from these refs/oids
    /// (`git fetch --shallow-exclude <ref>`). Each entry is sent as a wire
    /// `deepen-not <ref>`.
    pub deepen_not: Vec<String>,
    /// Convert a shallow repository back into a complete one
    /// (`git fetch --unshallow`). Drives the wire `deepen 0x7fffffff` request and
    /// removes the local `shallow` boundaries that get reported as `unshallow`.
    pub unshallow: bool,
    /// First fetch populating a remote-tracking namespace (clone): write `packed-refs`
    /// and `refs/remotes/{remote}/HEAD`.
    pub initial_remote_fetch: bool,
    /// Remote name paired with [`Self::initial_remote_fetch`] (e.g. `"origin"`).
    pub remote_name: Option<String>,
    /// Source label written into [`FETCH_HEAD`](https://git-scm.com/docs/git-fetch#_discussion) for
    /// anonymous URL/path fetches ([`crate::remote::Remote::from_url`] sets this automatically).
    pub fetch_source: Option<String>,
    /// Configured `remote.<name>.fetch` refspecs (for opportunistic tracking when [`Self::refspecs`]
    /// overrides them). [`crate::remote::Remote::fetch`] fills this for named remotes.
    pub configured_refspecs: Vec<String>,
    /// Set when [`Self::refspecs`] include explicit (non-wildcard) refspecs; drives FETCH_HEAD
    /// and stricter ref-update errors.
    pub explicit_refspec_fetch: bool,
    /// Clone-only reflog entries for `refs/remotes/{remote}/HEAD` (not remote branches).
    pub clone_reflog: Option<CloneReflog>,
    /// Diagnostic sink for [`crate::diagnostics::Trace::Network`] events.
    pub diagnostics: Option<crate::diagnostics::DiagnosticsHandle>,
    /// When true with [`Self::diagnostics`], fetch paths may emit network trace events.
    pub network_trace: bool,
    /// Override worker threads for index-pack after a received pack (`None` = config/default).
    pub index_pack_threads: Option<usize>,
    /// Global / embedder thread policy (`--threads`); applied when [`Self::index_pack_threads`] is unset.
    pub thread_policy: crate::parallel::ThreadPolicy,
    /// Discovery/config environment for pack ingest (`GIT_CONFIG_*`, `-c`); defaults to empty.
    pub environment: crate::environment::Environment,
    /// When true, spool the negotiated pack to disk and return without index-pack or ref updates.
    pub pack_spool_only: bool,
    /// Override the HTTP `Git-Protocol` header for this fetch (`None` → client default, usually v2).
    pub http_protocol_version: Option<u8>,
    /// When true, omit the `no-progress` capability/argument so the remote may
    /// send side-band channel-2 progress. Default false preserves today's wire bytes.
    pub remote_progress: bool,
}

impl Default for FetchOptions {
    fn default() -> Self {
        Self {
            refspecs: Vec::new(),
            negative_refspecs: Vec::new(),
            tags: TagMode::Following,
            prune: false,
            dry_run: false,
            depth: None,
            deepen_since: None,
            deepen_not: Vec::new(),
            unshallow: false,
            initial_remote_fetch: false,
            remote_name: None,
            fetch_source: None,
            configured_refspecs: Vec::new(),
            explicit_refspec_fetch: false,
            clone_reflog: None,
            diagnostics: None,
            network_trace: false,
            index_pack_threads: None,
            thread_policy: crate::parallel::ThreadPolicy::default(),
            environment: crate::environment::Environment::empty(),
            pack_spool_only: false,
            http_protocol_version: None,
            remote_progress: false,
        }
    }
}

/// Identity and message for clone reflog entries written by the library.
#[derive(Clone, Debug)]
pub struct CloneReflog {
    /// `"Name <email> <unix_time> <tz>"` formatted identity.
    pub identity: String,
    /// Reflog message (e.g. `clone: from <url>`).
    pub message: String,
}

impl Clone for FetchOptions {
    fn clone(&self) -> Self {
        Self {
            refspecs: self.refspecs.clone(),
            negative_refspecs: self.negative_refspecs.clone(),
            tags: self.tags,
            prune: self.prune,
            dry_run: self.dry_run,
            depth: self.depth,
            deepen_since: self.deepen_since.clone(),
            deepen_not: self.deepen_not.clone(),
            unshallow: self.unshallow,
            initial_remote_fetch: self.initial_remote_fetch,
            remote_name: self.remote_name.clone(),
            fetch_source: self.fetch_source.clone(),
            configured_refspecs: self.configured_refspecs.clone(),
            explicit_refspec_fetch: self.explicit_refspec_fetch,
            clone_reflog: self.clone_reflog.clone(),
            diagnostics: self.diagnostics.clone(),
            network_trace: self.network_trace,
            index_pack_threads: self.index_pack_threads,
            thread_policy: self.thread_policy,
            environment: self.environment.clone(),
            pack_spool_only: self.pack_spool_only,
            http_protocol_version: self.http_protocol_version,
            remote_progress: self.remote_progress,
        }
    }
}

impl FetchOptions {
    /// Whether this fetch carries any shallow/deepen request (an explicit
    /// `depth`/`deepen-since`/`deepen-not`/`unshallow`). Note this does NOT cover
    /// the "already shallow, fetching more of the same boundary" case — that is
    /// driven by the on-disk `shallow` file, checked separately by the fetch
    /// paths via [`crate::shallow::load_shallow_oids`].
    #[must_use]
    pub fn has_deepen_request(&self) -> bool {
        self.depth.is_some()
            || self
                .deepen_since
                .as_deref()
                .is_some_and(|v| !v.trim().is_empty())
            || self.deepen_not.iter().any(|v| !v.trim().is_empty())
            || self.unshallow
    }
}

/// Resolve whether a fetch should prune stale remote-tracking refs.
///
/// When `cli_prune` is [`Some`], that value wins (matching `--prune` /
/// `--no-prune`). Otherwise reads `remote.<remote>.prune`, then `fetch.prune`,
/// in Git precedence order. Unset keys default to `false`.
///
/// # Errors
///
/// Returns [`Error::Config`] when a configured prune key is not a valid boolean.
pub fn resolve_fetch_prune(
    config: &crate::config::ConfigSet,
    remote: &str,
    cli_prune: Option<bool>,
) -> Result<bool> {
    if let Some(enabled) = cli_prune {
        return Ok(enabled);
    }
    let remote_key = format!("remote.{remote}.prune");
    if let Some(value) = optional_config_bool(config, &remote_key)? {
        return Ok(value);
    }
    if let Some(value) = optional_config_bool(config, "fetch.prune")? {
        return Ok(value);
    }
    Ok(false)
}

fn optional_config_bool(config: &crate::config::ConfigSet, key: &str) -> Result<Option<bool>> {
    match config.get_bool(key) {
        None => Ok(None),
        Some(Ok(value)) => Ok(Some(value)),
        Some(Err(message)) => Err(Error::Config(crate::error::ConfigError::Other(format!(
            "invalid boolean for '{key}': {message}"
        )))),
    }
}

/// Object and byte counts for objects received during a fetch or clone.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TransferStats {
    /// Objects installed from the received pack (zero when nothing was transferred).
    pub objects: u64,
    /// Size in bytes of the received pack file (zero when nothing was transferred).
    pub bytes: u64,
    /// Objects by type, when the pack was indexed locally (`None` when packs were
    /// linked or copied from a local repository without being re-indexed).
    pub kinds: Option<crate::index_pack::ObjectKindCounts>,
}

impl TransferStats {
    /// Build stats from a pack installed under `objects/pack/`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] when the pack file cannot be read.
    pub fn from_ingested_pack(ingested: &crate::index_pack::IngestedPack) -> Result<Self> {
        transfer_stats_from_ingested(ingested)
    }
}

/// The structured result of a fetch, ready for the embedder's ref-store apply.
#[derive(Clone, Debug, Default)]
pub struct FetchOutcome {
    /// Per-ref resolved updates.
    pub updates: Vec<RefUpdate>,
    /// The remote's default branch (from `HEAD` symref), if known.
    pub default_branch: Option<String>,
    /// New shallow boundary commits the server reported (`shallow <oid>`), already
    /// applied to the local `shallow` file. The commits' parents are intentionally
    /// absent from the local object store after this fetch.
    pub new_shallow: Vec<ObjectId>,
    /// Commits the server reported as no longer shallow (`unshallow <oid>`), i.e.
    /// boundaries removed from the local `shallow` file because their history is
    /// now complete. Populated by a deepen / `--unshallow` fetch.
    pub new_unshallow: Vec<ObjectId>,
    /// Objects and pack bytes ingested during this fetch.
    pub transfer: TransferStats,
}

impl FetchOutcome {
    /// Count of remote branch refs represented in [`Self::updates`].
    #[must_use]
    pub fn branch_count(&self) -> u64 {
        self.updates
            .iter()
            .filter(|u| u.remote_ref.starts_with("refs/heads/"))
            .count() as u64
    }

    /// Count of remote tag refs represented in [`Self::updates`] (peeled lines excluded).
    #[must_use]
    pub fn tag_count(&self) -> u64 {
        self.updates
            .iter()
            .filter(|u| u.remote_ref.starts_with("refs/tags/") && !u.remote_ref.ends_with("^{}"))
            .count() as u64
    }
}

fn transfer_stats_from_ingested(
    ingested: &crate::index_pack::IngestedPack,
) -> Result<TransferStats> {
    let bytes = fs::metadata(&ingested.pack_path).map_err(Error::Io)?.len();
    Ok(TransferStats {
        objects: u64::try_from(ingested.object_ids.len()).unwrap_or(u64::MAX),
        bytes,
        kinds: Some(ingested.object_counts),
    })
}

fn transfer_stats_from_pack_dir(pack_dir: &Path) -> Result<TransferStats> {
    let mut stats = TransferStats::default();
    if !pack_dir.is_dir() {
        return Ok(stats);
    }
    for entry in fs::read_dir(pack_dir).map_err(Error::Io)? {
        let path = entry.map_err(Error::Io)?.path();
        if path.extension().is_none_or(|e| e != "pack") {
            continue;
        }
        stats.bytes += fs::metadata(&path).map_err(Error::Io)?.len();
        let idx = path.with_extension("idx");
        if idx.is_file() {
            // Linked or copied packfiles are already valid on disk; reading the
            // index entry count is enough for transfer stats (Git's local clone
            // path does not re-verify every object hash).
            let index = crate::pack::read_pack_index_no_verify(&idx)?;
            stats.objects += u64::try_from(index.len()).unwrap_or(u64::MAX);
        }
    }
    Ok(stats)
}

fn local_objects_pack_dir(git_dir: &Path) -> PathBuf {
    git_dir.join("objects").join("pack")
}

/// A single ref update requested by a push.
#[derive(Clone, Debug)]
pub struct PushRefSpec {
    /// The source object to push (`None` for a deletion).
    pub src: Option<ObjectId>,
    /// The destination ref on the remote (e.g. `refs/heads/main`).
    pub dst: String,
    /// Whether a non-fast-forward update is allowed.
    pub force: bool,
    /// Whether this update deletes the remote ref.
    pub delete: bool,
    /// Compare-and-swap expectation: the remote ref's current value must match
    /// this (force-with-lease). `None` disables the value check.
    pub expected_old: Option<ObjectId>,
    /// Force-with-lease expectation that the remote ref does **not** currently
    /// exist. When `true`, a push whose destination already exists on the remote
    /// is rejected as stale (used for "create only" pushes whose lease is the
    /// ref's absence). Independent of [`Self::expected_old`].
    pub expect_absent: bool,
}

/// Resolve the remote ref name for a push deletion from a short name or full ref.
///
/// When `spec` is a short name, a local `refs/heads/<spec>` wins, then an
/// advertised remote branch, then an advertised remote tag, otherwise
/// `refs/heads/<spec>` (same default as `git push <remote> :<branch>`).
#[must_use]
pub fn resolve_push_delete_ref(
    git_dir: &Path,
    remote_ref_names: &HashSet<String>,
    spec: &str,
) -> String {
    if spec.starts_with("refs/") {
        return spec.to_owned();
    }
    let branch = format!("refs/heads/{spec}");
    let tag = format!("refs/tags/{spec}");
    if crate::refs::resolve_ref(git_dir, &branch).is_ok() {
        return branch;
    }
    if remote_ref_names.contains(&branch) {
        return branch;
    }
    if remote_ref_names.contains(&tag) {
        return tag;
    }
    branch
}

/// Options controlling a push.
#[derive(Clone, Default)]
pub struct PushOptions {
    /// Apply all updates atomically (all-or-nothing).
    pub atomic: bool,
    /// Compute results without writing to the remote.
    pub dry_run: bool,
    /// When set, update `refs/remotes/<name>/...` in the local repo after a
    /// successful push (matching Git's post-push tracking ref maintenance).
    pub tracking_remote: Option<String>,
    /// Server-side push options to transmit (`git push --push-option <value>`).
    ///
    /// When non-empty, the negotiated capability list includes `push-options`
    /// and one `push-option <value>` pkt-line per entry is written after the
    /// ref-update command block and before the flush/pack. The remote exposes
    /// these to its hooks via `GIT_PUSH_OPTION_COUNT` / `GIT_PUSH_OPTION_<n>`.
    ///
    /// If this is non-empty but the remote `git-receive-pack` does not advertise
    /// the `push-options` capability, the push fails with
    /// [`crate::error::Error::PushOptionsUnsupported`] (matching Git).
    pub push_options: Vec<String>,
    /// Diagnostic sink for [`crate::diagnostics::Trace::Network`] events.
    pub diagnostics: Option<crate::diagnostics::DiagnosticsHandle>,
    /// When true with [`Self::diagnostics`], push paths may emit network trace events.
    pub network_trace: bool,
}

/// The structured result of a push. Reuses [`PushRefResult`] for per-ref status.
#[derive(Clone, Debug, Default)]
pub struct PushOutcome {
    /// Per-ref resolved results (status, old/new oid, reason).
    pub results: Vec<PushRefResult>,
}

pub use crate::pack_objects::{
    build_pack, build_pack_from_send_list, reachable_closure, PackBuildOptions, PackObjects,
    PackObjectsOptions, PackStats,
};

pub(crate) fn collect_objects_for_commits_only(
    odb: &Odb,
    commits: &[ObjectId],
    stop: Option<&HashSet<ObjectId>>,
) -> Result<Vec<ObjectId>> {
    let mut visited: HashSet<ObjectId> = HashSet::new();
    let mut ordered: Vec<ObjectId> = Vec::new();
    let mut queue: VecDeque<ObjectId> = VecDeque::new();

    let enqueue = |oid: ObjectId,
                   queue: &mut VecDeque<ObjectId>,
                   visited: &mut HashSet<ObjectId>,
                   ordered: &mut Vec<ObjectId>|
     -> bool {
        if stop.is_some_and(|s| s.contains(&oid)) {
            return false;
        }
        if visited.insert(oid) {
            ordered.push(oid);
            queue.push_back(oid);
            true
        } else {
            false
        }
    };

    for &c in commits {
        enqueue(c, &mut queue, &mut visited, &mut ordered);
    }

    while let Some(oid) = queue.pop_front() {
        let obj = odb.read(&oid)?;
        match obj.kind {
            ObjectKind::Commit => {
                let commit = parse_commit(&obj.data)?;
                enqueue(commit.tree, &mut queue, &mut visited, &mut ordered);
            }
            ObjectKind::Tree => {
                for entry in parse_tree(&obj.data)? {
                    if entry.mode == 0o160000 {
                        continue;
                    }
                    enqueue(entry.oid, &mut queue, &mut visited, &mut ordered);
                }
            }
            ObjectKind::Tag => {
                let tag = parse_tag(&obj.data)?;
                enqueue(tag.object, &mut queue, &mut visited, &mut ordered);
            }
            ObjectKind::Blob => {}
        }
    }
    Ok(ordered)
}

/// Annotated tag tips in `wants` must be packed even when their peeled commits
/// are already on the peer; the commit-only fast path does not walk tag objects.
fn merge_annotated_tag_wants(
    odb: &Odb,
    wants: &[ObjectId],
    send: &mut Vec<ObjectId>,
) -> Result<()> {
    let mut present: HashSet<ObjectId> = send.iter().copied().collect();
    for &want in wants {
        if present.contains(&want) {
            continue;
        }
        if odb.read(&want)?.kind == ObjectKind::Tag {
            send.push(want);
            present.insert(want);
        }
    }
    Ok(())
}

pub(crate) fn build_pack_push_fast(
    local_git_dir: &Path,
    odb: &Odb,
    wants: &[ObjectId],
    haves: &[ObjectId],
    opts: &PackBuildOptions,
) -> Result<Vec<u8>> {
    use crate::merge_base::{walk_commits_reachable_excluding_ancestors_of, ReachableWalkLimit};

    // Remote ref tips advertised on the wire may point at commits this clone
    // never fetched. Treat missing hide roots like `pack_object_select`'s
    // `skip_missing` walk: they must not abort packing unrelated new refs.
    let hide: Vec<ObjectId> = haves
        .iter()
        .copied()
        .filter(|tip| odb.exists(tip))
        .collect();

    let repo = crate::repo::Repository::open(local_git_dir, &RepositoryOptions::empty())?;
    let mut commit_set: HashSet<ObjectId> = HashSet::new();
    for &want in wants {
        let (_, commits) = walk_commits_reachable_excluding_ancestors_of(
            &repo,
            &[want],
            &hide,
            ReachableWalkLimit::Unlimited,
        )?;
        commit_set.extend(commits);
    }
    let commits: Vec<ObjectId> = commit_set.into_iter().collect();

    let mut exclude: HashSet<ObjectId> = HashSet::new();
    for &tip in &hide {
        exclude.insert(tip);
        let obj = odb.read(&tip)?;
        if obj.kind == ObjectKind::Commit {
            let commit = parse_commit(&obj.data)?;
            exclude.insert(commit.tree);
        }
    }
    // Only pack objects attached to the new commits themselves (no parent walks).
    let empty_haves = HashSet::new();
    let empty_name_hashes: HashMap<ObjectId, u32> = HashMap::new();
    collect_objects_for_commits_only(odb, &commits, Some(&exclude)).and_then(|mut send| {
        merge_annotated_tag_wants(odb, wants, &mut send)?;
        build_pack_from_send_list(odb, &send, &empty_haves, opts, &empty_name_hashes)
    })
}

pub(crate) use crate::pack_objects::build_pack_for_local_fetch;

fn pack_build_options_for_send(
    source_odb: &Odb,
    oids: &[ObjectId],
    base: &PackBuildOptions,
) -> PackBuildOptions {
    if oids.is_empty() || oids.len() > 512 {
        return *base;
    }
    let commits_tags_only = oids.iter().all(|oid| {
        source_odb
            .read(oid)
            .map(|obj| matches!(obj.kind, ObjectKind::Commit | ObjectKind::Tag))
            .unwrap_or(false)
    });
    if !commits_tags_only {
        return *base;
    }
    let mut fast = *base;
    fast.delta = false;
    fast.reuse_deltas = false;
    fast.thin = false;
    fast
}

fn ingest_pack_options_for_fetch(
    local_git_dir: &Path,
    fetch_opts: &FetchOptions,
) -> Result<crate::index_pack::IngestPackOptions> {
    let pack_config =
        crate::config::ConfigSet::load(&fetch_opts.environment, Some(local_git_dir), true)?;
    let index_threads = crate::parallel::index_pack_threads_for_ingest(
        fetch_opts.thread_policy,
        &pack_config,
        fetch_opts.index_pack_threads,
    )?;
    Ok(crate::index_pack::IngestPackOptions {
        fix_thin: true,
        skip_post_index_verify: true,
        threads: index_threads,
        merged_config: Some(pack_config),
        environment: fetch_opts.environment.clone(),
    })
}

/// Copy objects for a local fetch: link remote packfiles when possible, then pack any remainder.
#[allow(clippy::too_many_arguments)]
fn copy_objects_for_local_fetch(
    remote_git_dir: &Path,
    local_git_dir: &Path,
    remote_odb: &Odb,
    local_odb: &Odb,
    wants: &[ObjectId],
    haves: &[ObjectId],
    source_shallow: &HashSet<ObjectId>,
    have_shallow: &HashSet<ObjectId>,
    try_pack_link: bool,
    pack_build: &PackBuildOptions,
    fetch_opts: &FetchOptions,
    progress: &mut dyn ProgressSink,
) -> Result<(HashSet<ObjectId>, TransferStats)> {
    use crate::local_object_copy::{
        link_loose_objects_for_oids, linked_object_set, refresh_local_odb_after_link,
        source_objects_fully_packed, try_satisfy_via_object_link,
    };

    let remote_objects = remote_git_dir.join("objects");
    let local_objects = local_git_dir.join("objects");

    if !pack_build.thin
        && try_pack_link
        && haves.is_empty()
        && !wants.is_empty()
        && source_objects_fully_packed(&remote_objects)?
    {
        progress.start(ProgressPhase::CopyingObjects, Some(wants.len() as u64));
        if try_satisfy_via_object_link(&remote_objects, &local_objects, local_odb, wants, false)? {
            progress.set(wants.len() as u64);
            progress.finish();
            let oids = linked_object_set(local_odb, wants);
            let transfer = transfer_stats_from_pack_dir(&local_objects_pack_dir(local_git_dir))?;
            return Ok((oids, transfer));
        }
        progress.finish();
    }

    let have_tips: HashSet<ObjectId> = haves.iter().copied().collect();
    let send_set = collect_reachable_excluding_peer(
        remote_odb,
        wants,
        local_odb,
        &have_tips,
        false,
        source_shallow,
    )?;
    let send = &send_set.objects;

    if !pack_build.thin && try_pack_link {
        progress.start(ProgressPhase::CopyingObjects, Some(send.len() as u64));
        if try_satisfy_via_object_link(&remote_objects, &local_objects, local_odb, send, false)? {
            progress.set(send.len() as u64);
            progress.finish();
            let oids = linked_object_set(local_odb, send);
            let transfer = transfer_stats_from_pack_dir(&local_objects_pack_dir(local_git_dir))?;
            return Ok((oids, transfer));
        }
        progress.finish();
    }

    let mut still_missing = objects_missing_from_peer(local_odb, send);
    if still_missing.is_empty() {
        return Ok((linked_object_set(local_odb, send), TransferStats::default()));
    }

    // Hard-link loose objects only for incremental transfers; initial clones should
    // install a pack (or pack link), not thousands of loose files.
    if !haves.is_empty() {
        progress.start(ProgressPhase::CopyingObjects, Some(send.len() as u64));
        let linked_loose =
            link_loose_objects_for_oids(&remote_objects, &local_objects, &still_missing)?;
        if linked_loose > 0 {
            progress.inc(linked_loose as u64);
            refresh_local_odb_after_link(local_odb);
            still_missing.retain(|oid| !local_odb.exists(oid));
        }
        progress.finish();
        if still_missing.is_empty() {
            return Ok((linked_object_set(local_odb, send), TransferStats::default()));
        }
    }

    let pack_opts = pack_build_options_for_send(remote_odb, &still_missing, pack_build);
    progress.start(
        ProgressPhase::CopyingObjects,
        Some(still_missing.len() as u64),
    );
    let pack = build_pack_for_local_transfer(
        remote_odb,
        &send_set,
        &still_missing,
        local_odb,
        haves,
        source_shallow,
        have_shallow,
        &pack_opts,
    )?;
    let ingest_opts = ingest_pack_options_for_fetch(local_git_dir, fetch_opts)?;
    let ingested = crate::index_pack::ingest_received_pack(pack, local_odb, &ingest_opts)?;
    progress.inc(ingested.object_ids.len() as u64);
    progress.finish();
    refresh_local_odb_after_link(local_odb);
    let transfer = transfer_stats_from_ingested(&ingested)?;
    let mut out = linked_object_set(local_odb, send);
    out.extend(ingested.object_ids);
    Ok((out, transfer))
}

/// Copy objects for a local push: link source packfiles into the remote when possible, then pack any remainder.
#[allow(clippy::too_many_arguments)]
fn copy_objects_for_local_push(
    local_git_dir: &Path,
    remote_git_dir: &Path,
    local_odb: &Odb,
    remote_odb: &Odb,
    wants: &[ObjectId],
    haves: &[ObjectId],
    source_shallow: &HashSet<ObjectId>,
    have_shallow: &HashSet<ObjectId>,
    opts: &PackBuildOptions,
) -> Result<()> {
    use crate::local_object_copy::{
        link_loose_objects_for_oids, refresh_local_odb_after_link, try_satisfy_via_object_link,
    };

    let local_objects = local_git_dir.join("objects");
    let remote_objects = remote_git_dir.join("objects");

    let have_tips: HashSet<ObjectId> = haves.iter().copied().collect();
    let send_set = collect_reachable_excluding_peer(
        local_odb,
        wants,
        remote_odb,
        &have_tips,
        false,
        source_shallow,
    )?;
    let send = &send_set.objects;

    if !opts.thin
        && try_satisfy_via_object_link(&local_objects, &remote_objects, remote_odb, send, false)?
    {
        return Ok(());
    }

    let mut still_missing = objects_missing_from_peer(remote_odb, send);
    if still_missing.is_empty() {
        return Ok(());
    }

    if !haves.is_empty() {
        let linked_loose =
            link_loose_objects_for_oids(&local_objects, &remote_objects, &still_missing)?;
        if linked_loose > 0 {
            refresh_local_odb_after_link(remote_odb);
            still_missing.retain(|oid| !remote_odb.exists(oid));
        }
        if still_missing.is_empty() {
            return Ok(());
        }
    }

    let pack_opts = pack_build_options_for_send(local_odb, &still_missing, opts);
    let pack = build_pack_for_local_transfer(
        local_odb,
        &send_set,
        &still_missing,
        remote_odb,
        haves,
        source_shallow,
        have_shallow,
        &pack_opts,
    )?;
    crate::index_pack::ingest_received_pack(
        pack,
        remote_odb,
        &crate::index_pack::IngestPackOptions {
            fix_thin: pack_opts.thin,
            ..Default::default()
        },
    )?;
    refresh_local_odb_after_link(remote_odb);
    Ok(())
}

/// The objects of `send` that `peer_odb` does not store on disk.
///
/// Uses the on-disk check rather than [`Odb::exists`], which reports the empty
/// tree as present even without an object file: the result may be packed
/// as-is, and the peer must end up with every object it references.
fn objects_missing_from_peer(peer_odb: &Odb, send: &[ObjectId]) -> Vec<ObjectId> {
    peer_odb.reprepare_local_packs();
    send.iter()
        .copied()
        .filter(|oid| !peer_odb.exists_materialized_local_without_rescan(oid))
        .collect()
}

/// Build the pack that copies `missing` (a subset of `send_set`) into the peer.
///
/// With no `haves` and a non-thin pack, `missing` is already the exact set the
/// peer lacks (the walk stopped at every object the peer stores), so it is
/// packed as-is. Re-enumerating from `missing` instead would walk its whole
/// closure and re-send objects just linked into the peer, such as every packed
/// object reachable from a loose annotated tag. Otherwise defer to
/// [`build_pack_for_local_fetch`], which negotiates against the peer's `haves`.
#[allow(clippy::too_many_arguments)]
fn build_pack_for_local_transfer(
    source_odb: &Odb,
    send_set: &PeerSendSet,
    missing: &[ObjectId],
    peer_odb: &Odb,
    haves: &[ObjectId],
    source_shallow: &HashSet<ObjectId>,
    have_shallow: &HashSet<ObjectId>,
    opts: &PackBuildOptions,
) -> Result<Vec<u8>> {
    if haves.is_empty() && !opts.thin {
        return crate::pack_objects::build_pack_from_send_list(
            source_odb,
            missing,
            &HashSet::new(),
            opts,
            &send_set.blob_name_hashes,
        );
    }
    build_pack_for_local_fetch(
        source_odb,
        missing,
        peer_odb,
        haves,
        source_shallow,
        have_shallow,
        opts,
    )
}

/// Objects a local transfer must send, from [`collect_reachable_excluding_peer`].
struct PeerSendSet {
    /// Objects missing from the peer, in walk order.
    objects: Vec<ObjectId>,
    /// Pack name hash of each blob's path, for delta-window ordering.
    blob_name_hashes: HashMap<ObjectId, u32>,
}

/// Like [`collect_reachable_excluding`], but treats every object already stored in
/// `peer_odb` as a boundary (plus explicit `peer_boundary` tips). Used for local
/// file fetch/push so incremental transfers do not walk full history from `haves`.
fn collect_reachable_excluding_peer(
    source_odb: &Odb,
    roots: &[ObjectId],
    peer_odb: &Odb,
    peer_boundary: &HashSet<ObjectId>,
    skip_missing: bool,
    shallow_grafts: &HashSet<ObjectId>,
) -> Result<PeerSendSet> {
    use crate::pack_name_hash::pack_name_hash_extend;

    let mut visited: HashSet<ObjectId> = HashSet::new();
    let mut ordered: Vec<ObjectId> = Vec::new();
    let mut blob_name_hashes: HashMap<ObjectId, u32> = HashMap::new();
    // Each queued object carries the name hash of its directory prefix ("dir/").
    let mut queue: VecDeque<(ObjectId, u32)> = VecDeque::new();

    // The peer is not written during the walk, so one pack-directory refresh is
    // enough; a stale miss only means sending an object the peer already has.
    peer_odb.reprepare_local_packs();
    let at_boundary = |oid: ObjectId| {
        peer_boundary.contains(&oid) || peer_odb.exists_materialized_local_without_rescan(&oid)
    };

    // `visited` also remembers boundary objects so each object is probed once.
    // Blobs named by tree entries are recorded without being read: they have no
    // outgoing links, and a missing one still fails when the pack is written.
    let enqueue = |oid: ObjectId,
                   prefix_hash: Option<u32>,
                   queue: &mut VecDeque<(ObjectId, u32)>,
                   visited: &mut HashSet<ObjectId>,
                   ordered: &mut Vec<ObjectId>|
     -> bool {
        if !visited.insert(oid) || at_boundary(oid) {
            return false;
        }
        ordered.push(oid);
        if let Some(prefix_hash) = prefix_hash {
            queue.push_back((oid, prefix_hash));
        }
        true
    };

    for &root in roots {
        enqueue(root, Some(0), &mut queue, &mut visited, &mut ordered);
    }

    while let Some((oid, prefix_hash)) = queue.pop_front() {
        let obj = match source_odb.read(&oid) {
            Ok(o) => o,
            Err(_) if skip_missing => continue,
            Err(e) => return Err(e),
        };
        match obj.kind {
            ObjectKind::Commit => {
                let commit = parse_commit(&obj.data)?;
                if !shallow_grafts.contains(&oid) {
                    for parent in commit.parents {
                        enqueue(parent, Some(0), &mut queue, &mut visited, &mut ordered);
                    }
                }
                enqueue(commit.tree, Some(0), &mut queue, &mut visited, &mut ordered);
            }
            ObjectKind::Tree => {
                for entry in parse_tree(&obj.data)? {
                    if entry.mode == 0o160000 {
                        continue;
                    }
                    let path_hash = pack_name_hash_extend(prefix_hash, &entry.name);
                    if entry.mode & 0o170000 == 0o040000 {
                        let dir_hash = pack_name_hash_extend(path_hash, b"/");
                        enqueue(
                            entry.oid,
                            Some(dir_hash),
                            &mut queue,
                            &mut visited,
                            &mut ordered,
                        );
                    } else if enqueue(entry.oid, None, &mut queue, &mut visited, &mut ordered) {
                        blob_name_hashes.insert(entry.oid, path_hash);
                    }
                }
            }
            ObjectKind::Tag => {
                let tag = parse_tag(&obj.data)?;
                enqueue(
                    tag.object,
                    Some(prefix_hash),
                    &mut queue,
                    &mut visited,
                    &mut ordered,
                );
            }
            ObjectKind::Blob => {}
        }
    }

    Ok(PeerSendSet {
        objects: ordered,
        blob_name_hashes,
    })
}

/// Expand a thin pack by appending missing ref-delta bases from `odb`, matching
/// `git index-pack --fix-thin`.
/// Expand a thin on-disk pack, returning the path to the pack bytes to index.
///
/// When the pack is not thin, returns `pack_path` unchanged. Otherwise writes a
/// fixed pack alongside the input and removes the thin temp file.
pub(crate) fn fix_thin_pack_path(pack_path: &Path, odb: &Odb) -> Result<std::path::PathBuf> {
    use crate::pack_map::PackData;
    use std::ops::Deref;

    let mapped = PackData::open(pack_path)?;
    if !crate::unpack_objects::pack_is_thin(mapped.deref(), odb.hash_algo()) {
        return Ok(pack_path.to_path_buf());
    }
    let data = std::fs::read(pack_path).map_err(Error::Io)?;
    let fixed = fix_thin_pack(data, odb)?;
    let out = pack_path.with_extension("fixed");
    std::fs::write(&out, &fixed).map_err(Error::Io)?;
    let _ = std::fs::remove_file(pack_path);
    Ok(out)
}

pub(crate) fn fix_thin_pack(mut pack: Vec<u8>, odb: &Odb) -> Result<Vec<u8>> {
    let algo = odb.hash_algo();
    let hb = algo.len();
    if !crate::unpack_objects::pack_is_thin(&pack, algo) {
        return Ok(pack);
    }
    if pack.len() < 12 + hb {
        return Err(Error::CorruptObject(
            "thin pack fix: pack too small".to_owned(),
        ));
    }
    let mut missing: Vec<ObjectId> = Vec::new();
    let mut seen = HashSet::new();
    let in_pack = in_pack_whole_object_ids(&pack, algo);
    let mut pos = 12usize;
    let pack_end = pack.len() - hb;
    while pos < pack_end {
        let (type_code, size, header_len) = read_pack_type_size_at(&pack, pos)?;
        let payload_start = pos + header_len;
        match type_code {
            7 => {
                if payload_start + hb > pack_end {
                    return Err(Error::CorruptObject(
                        "thin pack fix: truncated ref-delta".to_owned(),
                    ));
                }
                let base_oid = ObjectId::from_bytes(&pack[payload_start..payload_start + hb])?;
                if !in_pack.contains(&base_oid) && seen.insert(base_oid) {
                    missing.push(base_oid);
                }
                pos = payload_start + hb + zlib_skip(&pack[payload_start + hb..], size)?;
            }
            6 => {
                let (base_len, _) = read_ofs_delta_prefix_len(&pack[payload_start..])?;
                pos =
                    payload_start + base_len + zlib_skip(&pack[payload_start + base_len..], size)?;
            }
            1..=4 => {
                pos = payload_start + zlib_skip(&pack[payload_start..], size)?;
            }
            _ => {
                return Err(Error::CorruptObject(format!(
                    "thin pack fix: unknown type {type_code}"
                )));
            }
        }
    }
    if missing.is_empty() {
        return Ok(pack);
    }
    pack.truncate(pack.len() - hb);
    crate::pack_objects::append_whole_objects_from_odb(&mut pack, odb, &missing)?;
    let old_count = u32::from_be_bytes(
        pack[8..12]
            .try_into()
            .map_err(|_| Error::CorruptObject("thin pack fix: bad object count".to_owned()))?,
    );
    let added = u32::try_from(missing.len())
        .map_err(|_| Error::CorruptObject("thin pack fix: object count overflow".to_owned()))?;
    let new_count = old_count
        .checked_add(added)
        .ok_or_else(|| Error::CorruptObject("thin pack fix: object count overflow".to_owned()))?;
    pack[8..12].copy_from_slice(&new_count.to_be_bytes());
    crate::pack_objects::append_pack_trailer(&mut pack, algo);
    Ok(pack)
}

fn in_pack_whole_object_ids(pack: &[u8], algo: HashAlgo) -> HashSet<ObjectId> {
    let mut out = HashSet::new();
    let hb = algo.len();
    if pack.len() < 12 + hb {
        return out;
    }
    let pack_end = pack.len() - hb;
    let mut pos = 12usize;
    while pos < pack_end {
        let Ok((type_code, size, header_len)) = read_pack_type_size_at(pack, pos) else {
            break;
        };
        let payload_start = pos + header_len;
        if (1..=4).contains(&type_code) {
            if let Ok(data) = zlib_decompress_fixed(&pack[payload_start..], size) {
                if let Ok(kind) = pack_type_code_to_kind(type_code) {
                    let oid = hash::hash_object(algo, kind, &data);
                    out.insert(oid);
                }
            }
        }
        let advance = match type_code {
            7 if payload_start + hb <= pack_end => {
                header_len + hb + zlib_skip(&pack[payload_start + hb..], size).unwrap_or(0)
            }
            6 => {
                let base_len = read_ofs_delta_prefix_len(&pack[payload_start..])
                    .map(|(n, _)| n)
                    .unwrap_or(0);
                header_len
                    + base_len
                    + zlib_skip(&pack[payload_start + base_len..], size).unwrap_or(0)
            }
            1..=4 => header_len + zlib_skip(&pack[payload_start..], size).unwrap_or(0),
            _ => break,
        };
        pos += advance;
    }
    out
}

fn pack_type_code_to_kind(code: u8) -> Result<ObjectKind> {
    match code {
        1 => Ok(ObjectKind::Commit),
        2 => Ok(ObjectKind::Tree),
        3 => Ok(ObjectKind::Blob),
        4 => Ok(ObjectKind::Tag),
        _ => Err(Error::CorruptObject(format!("bad pack type {code}"))),
    }
}

fn read_pack_type_size_at(pack: &[u8], start: usize) -> Result<(u8, usize, usize)> {
    let first = *pack
        .get(start)
        .ok_or_else(|| Error::CorruptObject("truncated pack header".to_owned()))?;
    let type_code = (first >> 4) & 0x7;
    let mut size = (first & 0x0f) as usize;
    let mut pos = start + 1;
    let mut shift = 4u32;
    let mut cur = first;
    while cur & 0x80 != 0 {
        cur = *pack
            .get(pos)
            .ok_or_else(|| Error::CorruptObject("truncated pack header".to_owned()))?;
        pos += 1;
        size |= ((cur & 0x7f) as usize) << shift;
        shift += 7;
    }
    Ok((type_code, size, pos - start))
}

fn read_ofs_delta_prefix_len(bytes: &[u8]) -> Result<(usize, u64)> {
    let mut pos = 0usize;
    let mut c = *bytes
        .get(pos)
        .ok_or_else(|| Error::CorruptObject("truncated ofs-delta".to_owned()))?;
    pos += 1;
    let mut value = (c & 0x7f) as u64;
    while c & 0x80 != 0 {
        c = *bytes
            .get(pos)
            .ok_or_else(|| Error::CorruptObject("truncated ofs-delta".to_owned()))?;
        pos += 1;
        value = (value + 1) << 7 | (c & 0x7f) as u64;
    }
    Ok((pos, value))
}

fn zlib_skip(bytes: &[u8], expected_size: usize) -> Result<usize> {
    use flate2::read::ZlibDecoder;
    use std::io::Read;
    let mut dec = ZlibDecoder::new(bytes);
    let mut tmp = Vec::with_capacity(expected_size);
    dec.read_to_end(&mut tmp)
        .map_err(|e| Error::Zlib(e.to_string()))?;
    Ok(dec.total_in() as usize)
}

fn zlib_decompress_fixed(bytes: &[u8], expected_size: usize) -> Result<Vec<u8>> {
    use flate2::read::ZlibDecoder;
    use std::io::Read;
    let mut dec = ZlibDecoder::new(bytes);
    let mut out = Vec::with_capacity(expected_size);
    dec.read_to_end(&mut out)
        .map_err(|e| Error::Zlib(e.to_string()))?;
    if out.len() != expected_size {
        return Err(Error::CorruptObject(format!(
            "zlib size mismatch: got {} expected {expected_size}",
            out.len()
        )));
    }
    Ok(out)
}

/// `fetch_local` function.
pub fn fetch_local(
    local_git_dir: &Path,
    remote_git_dir: &Path,
    opts: &FetchOptions,
) -> Result<FetchOutcome> {
    let mut opts = opts.clone();
    fetch_local_with_progress(local_git_dir, remote_git_dir, &mut opts, &mut NoProgress)
}

/// Match remote refs to fetch refspecs (shared by local, wire, and bundle fetch).
pub(crate) fn build_fetch_matched_set(
    opts: &mut FetchOptions,
    remote_refs: &[(String, ObjectId)],
) -> Result<(Vec<MatchedRef>, Vec<RefspecItem>, Vec<RefspecItem>)> {
    use crate::fetch_request::{FetchRefspecInput, FetchRefspecPlan};

    let plan = FetchRefspecPlan::resolve(
        &FetchRefspecInput {
            user_refspecs: opts.refspecs.clone(),
            configured_refspecs: opts.configured_refspecs.clone(),
            remote_name: opts.remote_name.clone(),
        },
        remote_refs,
    )?;
    opts.explicit_refspec_fetch = plan.is_explicit();

    let mut negatives = match &plan {
        FetchRefspecPlan::Pattern { negatives, .. }
        | FetchRefspecPlan::Explicit { negatives, .. } => negatives.clone(),
    };
    for spec in &opts.negative_refspecs {
        let item = parse_fetch_refspec(spec)
            .map_err(|e| Error::Message(format!("invalid negative refspec '{spec}': {e}")))?;
        negatives.push(item);
    }

    let positive = match &plan {
        FetchRefspecPlan::Pattern { positive, .. } => positive.clone(),
        FetchRefspecPlan::Explicit { .. } => opts
            .refspecs
            .iter()
            .filter_map(|s| parse_fetch_refspec(s).ok())
            .filter(|i| !i.negative && !i.pattern && !i.matching)
            .collect(),
    };

    let matched = crate::fetch_request::matched_refs_from_plan(&plan, remote_refs)?;
    Ok((matched, positive, negatives))
}

/// Enforce Git fetch ref-update rules for explicit refspec fetches.
pub(crate) fn enforce_fetch_ref_update(
    local_ref: &str,
    old: Option<ObjectId>,
    new: ObjectId,
    mode: UpdateMode,
    opts: &FetchOptions,
    local_repo: Option<&crate::repo::Repository>,
) -> Result<()> {
    if opts.dry_run {
        return Ok(());
    }
    let would_write = matches!(
        mode,
        UpdateMode::New | UpdateMode::FastForward | UpdateMode::Forced
    );
    if let Some(short) = local_ref.strip_prefix("refs/heads/") {
        if old.is_some()
            && old != Some(new)
            && (would_write || mode == UpdateMode::NonFastForwardRejected)
        {
            if let Some(repo) = local_repo {
                if crate::worktree::find_worktree_with_branch_checked_out(repo, short, None)?
                    .is_some()
                {
                    return Err(Error::FetchIntoCheckedOutBranch {
                        branch: short.to_owned(),
                    });
                }
            }
        }
    }
    if mode == UpdateMode::NonFastForwardRejected && opts.explicit_refspec_fetch {
        return Err(Error::FetchNonFastForward {
            local_ref: local_ref.to_owned(),
        });
    }
    Ok(())
}

/// Like [`fetch_local`], reporting object copy progress through `progress`.
pub fn fetch_local_with_progress(
    local_git_dir: &Path,
    remote_git_dir: &Path,
    opts: &mut FetchOptions,
    progress: &mut dyn Progress,
) -> Result<FetchOutcome> {
    // Validate that the remote is actually a Git repository: a missing or
    // non-repo path must error (e.g. cloning a bad source) rather than silently
    // fetching nothing. A repo has an `objects` directory (bare or `.git`).
    if !remote_git_dir.join("objects").is_dir() {
        return Err(Error::Message(format!(
            "could not find repository at '{}'",
            remote_git_dir.display()
        )));
    }

    let local_odb = open_odb(local_git_dir);
    let remote_odb = open_odb(remote_git_dir);

    // 1. Enumerate remote refs (with HEAD symref for the default branch).
    let remote_entries = crate::remote::list_refs_from_git_dir_for_fetch(
        remote_git_dir,
        Some(&remote_odb),
        &opts.refspecs,
    )
    .map_err(crate::error::Error::from)?;

    let mut default_branch = None;
    // remote ref name -> oid (excluding HEAD and peeled `^{}` entries).
    let mut remote_refs: Vec<(String, ObjectId)> = Vec::new();
    for entry in &remote_entries {
        if entry.name == "HEAD" {
            default_branch = entry
                .symref_target
                .as_ref()
                .map(|t| t.strip_prefix("refs/heads/").unwrap_or(t).to_owned());
            continue;
        }
        if entry.name.ends_with("^{}") {
            continue;
        }
        if !crate::refs::is_valid_fetch_advertised_ref(&entry.name) {
            continue;
        }
        remote_refs.push((entry.name.clone(), entry.oid));
    }

    let (mut matched, positive, negatives) = build_fetch_matched_set(opts, &remote_refs)?;
    let mut matched_oids: HashSet<ObjectId> = matched.iter().map(|m| m.oid).collect();
    let mut seen_remote_ref: HashSet<String> =
        matched.iter().map(|m| m.remote_ref.clone()).collect();

    // TagMode: add tags. We need the closure of objects already being fetched to
    // decide "Following".
    let local_shallow = crate::shallow::load_shallow_boundaries(local_git_dir);
    let remote_shallow = crate::shallow::load_shallow_boundaries(remote_git_dir);

    let mut tag_shallow = local_shallow.clone();
    tag_shallow.extend(remote_shallow.iter().copied());

    let mut following_only = HashSet::new();
    if matches!(opts.tags, TagMode::All | TagMode::Following) {
        let tags_present = if opts.tags == TagMode::Following {
            crate::fetch::tags_already_local(local_git_dir, &remote_refs)
        } else {
            HashSet::new()
        };
        following_only = apply_tag_mode(
            opts.tags,
            &remote_refs,
            &remote_odb,
            &tag_shallow,
            &negatives,
            &tags_present,
            &mut matched,
            &mut matched_oids,
            &mut seen_remote_ref,
        )?;
    }

    let fetch_commit_oids: HashSet<ObjectId> = matched
        .iter()
        .filter(|m| !m.is_tag)
        .map(|m| m.oid)
        .collect();
    let mut following_tag_wants =
        crate::fetch::following_tag_wants_with_local_peel(&matched, &following_only, &local_odb);
    let shallow_request = opts.has_deepen_request() || !local_shallow.is_empty();
    if shallow_request {
        let peel_set = crate::fetch::following_peel_commit_set_for_shallow(
            &remote_odb,
            &fetch_commit_oids,
            opts.depth,
            &tag_shallow,
        );
        following_tag_wants.extend(
            crate::fetch::following_tag_wants_peel_among_fetched_commits(
                &matched,
                &following_only,
                &peel_set,
            ),
        );
    }

    // 3. Determine wants (matched oids not present locally) and haves (current
    //    local tracking-ref tips) and copy the minimal object set.
    let mut wants: Vec<ObjectId> = if shallow_request {
        matched_oids
            .iter()
            .copied()
            .filter(|oid| !following_only.contains(oid) || following_tag_wants.contains(oid))
            .collect()
    } else {
        matched_oids
            .iter()
            .copied()
            .filter(|oid| !local_odb.exists(oid))
            .collect()
    };

    let shallow_wants: Vec<ObjectId> = matched
        .iter()
        .filter(|m| !m.is_tag)
        .map(|m| m.oid)
        .filter(|oid| wants.contains(oid))
        .collect();

    let mut haves: Vec<ObjectId> = Vec::new();
    let mut have_seen: HashSet<ObjectId> = HashSet::new();
    for m in &matched {
        if let Some(local_ref) = &m.local_ref {
            if let Ok(old) = crate::refs::resolve_ref(local_git_dir, local_ref) {
                if have_seen.insert(old) {
                    haves.push(old);
                }
            }
        }
    }

    let mut pack_oids: HashSet<ObjectId> = HashSet::new();
    let mut transfer = TransferStats::default();
    let mut shallow_new: Vec<ObjectId> = Vec::new();
    let mut shallow_unshallow: Vec<ObjectId> = Vec::new();
    let mut copy_source_shallow = remote_shallow.clone();
    if opts.has_deepen_request() {
        let remote_repo =
            crate::repo::Repository::open(remote_git_dir, &RepositoryOptions::empty())?;
        let req = crate::serve::ShallowRequest {
            client_shallow: local_shallow.iter().copied().collect(),
            depth: if opts.unshallow {
                Some(crate::shallow::INFINITE_DEPTH)
            } else {
                opts.depth
            },
            deepen_since: opts
                .deepen_since
                .as_deref()
                .and_then(|s| s.parse::<i64>().ok()),
            deepen_not: Vec::new(),
            deepen_relative: false,
        };
        let resp = crate::serve::compute_shallow_response(&remote_repo, &shallow_wants, &req)?;
        shallow_new = resp.shallow;
        shallow_unshallow = resp.unshallow;
        copy_source_shallow = resp.pack_shallow_grafts;
        if opts.tags == TagMode::Following {
            if let Some(depth) = opts.depth.filter(|d| *d > 0) {
                let mut effective_shallow = tag_shallow.clone();
                effective_shallow.extend(shallow_new.iter().copied());
                for oid in &shallow_unshallow {
                    effective_shallow.remove(oid);
                }
                let fetch_commits = crate::fetch::shallow_fetched_commit_oids(
                    &remote_odb,
                    &fetch_commit_oids.iter().copied().collect::<Vec<_>>(),
                    depth,
                    &effective_shallow,
                );
                for m in &matched {
                    if !m.is_tag || local_odb.exists(&m.oid) || wants.contains(&m.oid) {
                        continue;
                    }
                    let peel = remote_odb
                        .peel_tags(m.oid)
                        .ok()
                        .map(|(p, _)| p)
                        .unwrap_or(m.oid);
                    if fetch_commits.contains(&peel) {
                        wants.push(m.oid);
                    }
                }
            }
        }
        for oid in &fetch_commit_oids {
            if !wants.contains(oid) {
                wants.push(*oid);
            }
        }
    }
    if !wants.is_empty() && !opts.dry_run {
        let remote_cfg = config_for_git_dir(remote_git_dir);
        let pack_opts = PackBuildOptions::for_local_copy(remote_cfg.as_ref());
        let (oids, fetch_transfer) = copy_objects_for_local_fetch(
            remote_git_dir,
            local_git_dir,
            &remote_odb,
            &local_odb,
            &wants,
            &haves,
            &copy_source_shallow,
            &local_shallow,
            !opts.has_deepen_request(),
            &pack_opts,
            opts,
            progress,
        )?;
        pack_oids = oids;
        transfer = fetch_transfer;
    }

    if opts.initial_remote_fetch
        && !remote_shallow.is_empty()
        && !opts.has_deepen_request()
        && !opts.dry_run
    {
        let remote_shallow_vec: Vec<ObjectId> = remote_shallow.iter().copied().collect();
        crate::shallow::apply_shallow_updates(local_git_dir, &remote_shallow_vec, &[])?;
    }
    if (!shallow_new.is_empty() || !shallow_unshallow.is_empty()) && !opts.dry_run {
        crate::shallow::apply_shallow_updates(local_git_dir, &shallow_new, &shallow_unshallow)?;
    }

    if opts.tags == TagMode::Following {
        let boundaries = crate::shallow::load_shallow_boundaries(local_git_dir);
        crate::fetch::retain_following_tags(&local_odb, &mut matched, &pack_oids, &boundaries)?;
    }

    // 4. Classify and apply ref updates. Ancestry checks use the local repo,
    //    which now contains the fetched objects.
    let local_repo = if opts.dry_run {
        None
    } else {
        crate::repo::Repository::open(local_git_dir, &RepositoryOptions::empty()).ok()
    };

    let mut updates: Vec<RefUpdate> = Vec::new();

    // Prune BEFORE writing the new tips. A stale tracking ref stored as a file
    // (e.g. `refs/remotes/origin/a`) otherwise blocks creating a nested ref the
    // same fetch introduces (`refs/remotes/origin/a/b`) with a "File exists"
    // directory/file conflict (matches `git fetch --prune` ordering).
    let mut prune_batch: Vec<crate::refs::store::RefUpdate> = Vec::new();
    if opts.prune {
        prune_tracking_refs(
            local_git_dir,
            &positive,
            &remote_refs,
            opts.dry_run,
            &mut updates,
            if opts.dry_run {
                None
            } else {
                Some(&mut prune_batch)
            },
        )?;
    }
    if !opts.dry_run && !prune_batch.is_empty() {
        crate::refs::commit_ref_store_batch(local_git_dir, &prune_batch)?;
    }

    let packed = crate::refs::PackedRefs::load(local_git_dir)?;

    let mut store_batch: Vec<crate::refs::store::RefUpdate> = Vec::new();
    for m in &matched {
        let Some(local_ref) = &m.local_ref else {
            // dst empty: fetched but not stored. Report as a no-store update.
            updates.push(RefUpdate {
                remote_ref: m.remote_ref.clone(),
                local_ref: None,
                old_oid: None,
                new_oid: Some(m.oid),
                mode: UpdateMode::NoChangeNeeded,
                note: Some("not stored (empty destination)".to_owned()),
            });
            continue;
        };

        let old = crate::refs::resolve_ref_cached(local_git_dir, local_ref, &packed)
            .ok()
            .flatten();
        let mode = classify_update(old.as_ref(), &m.oid, m.force, m.is_tag, local_repo.as_ref());

        let write = matches!(
            mode,
            UpdateMode::New | UpdateMode::FastForward | UpdateMode::Forced
        );
        enforce_fetch_ref_update(local_ref, old, m.oid, mode, opts, local_repo.as_ref())?;
        if write && !opts.dry_run {
            if !crate::refs::is_valid_storable_ref_name(local_ref) {
                updates.push(RefUpdate {
                    remote_ref: m.remote_ref.clone(),
                    local_ref: Some(local_ref.clone()),
                    old_oid: old,
                    new_oid: Some(m.oid),
                    mode,
                    note: Some("skipped (invalid ref name)".to_owned()),
                });
                continue;
            }
            if m.is_tag && !ref_target_exists(&local_odb, &remote_odb, m.oid) {
                updates.push(RefUpdate {
                    remote_ref: m.remote_ref.clone(),
                    local_ref: Some(local_ref.clone()),
                    old_oid: old,
                    new_oid: Some(m.oid),
                    mode: UpdateMode::NoChangeNeeded,
                    note: Some("skipped (tag target not present locally)".to_owned()),
                });
                continue;
            }
            store_batch.push(crate::refs::store::RefUpdate {
                name: local_ref.clone(),
                new_value: Some(crate::refs::store::RawRef::Direct(m.oid)),
                expected: crate::refs::store::Expected::Any,
                reflog: None,
                flags: Default::default(),
            });
        }

        updates.push(RefUpdate {
            remote_ref: m.remote_ref.clone(),
            local_ref: Some(local_ref.clone()),
            old_oid: old,
            new_oid: Some(m.oid),
            mode,
            note: None,
        });
    }

    if let Some(remote) = opts.remote_name.as_deref() {
        if !opts.dry_run {
            if let Some(branch) = default_branch.as_deref() {
                let head_ref = format!("refs/remotes/{remote}/HEAD");
                if crate::refs::resolve_ref(local_git_dir, &head_ref).is_err() {
                    let target = format!("refs/remotes/{remote}/{branch}");
                    let target_will_exist = store_batch.iter().any(|u| u.name == target)
                        || crate::refs::resolve_ref(local_git_dir, &target).is_ok();
                    if target_will_exist {
                        store_batch.push(crate::refs::store::RefUpdate {
                            name: head_ref,
                            new_value: Some(crate::refs::store::RawRef::Symbolic(target)),
                            expected: crate::refs::store::Expected::Any,
                            reflog: None,
                            flags: Default::default(),
                        });
                    }
                }
            }
        }
    }
    if !opts.dry_run && !store_batch.is_empty() {
        crate::refs::commit_ref_store_batch(local_git_dir, &store_batch)?;
    }

    crate::fetch::finish_initial_remote_fetch_layout(
        local_git_dir,
        opts,
        default_branch.as_deref(),
    )?;
    if let Some(remote) = opts.remote_name.as_deref() {
        crate::fetch::ensure_remote_tracking_head_symref(
            local_git_dir,
            remote,
            default_branch.as_deref(),
            opts.dry_run,
        )?;
    }
    finish_anonymous_url_fetch(
        local_git_dir,
        opts,
        default_branch.as_deref(),
        &remote_refs,
        &matched,
    )?;
    let new_shallow = crate::shallow::dedupe_shallow_list(if opts.has_deepen_request() {
        shallow_new
    } else if opts.initial_remote_fetch {
        remote_shallow.iter().copied().collect()
    } else {
        Vec::new()
    });
    Ok(FetchOutcome {
        updates,
        default_branch,
        new_shallow,
        new_unshallow: shallow_unshallow,
        transfer,
    })
}

/// Push refs and objects from one on-disk git repository into another, entirely
/// in-process (no subprocess, no wire protocol).
///
/// This is the local / `file://` push (send-pack) counterpart to
/// [`fetch_local`]. For each [`PushRefSpec`] it:
///
/// 1. Resolves the source oid from the LOCAL repo (for a non-delete update) and
///    reads the remote's current value of `dst`.
/// 2. Enforces the update rules and produces a [`PushRefResult`] with the right
///    [`crate::push_report::PushRefStatus`]:
///    * `expected_old` set and mismatching the remote's current value →
///      [`PushRefStatus::RejectStale`] (compare-and-swap / force-with-lease).
///    * deletion → succeed when present, or [`PushRefStatus::UpToDate`] when the
///      ref is already gone.
///    * non-fast-forward (remote current is not an ancestor of the source)
///      without `force` → [`PushRefStatus::RejectNonFastForward`]; with `force`
///      it is accepted and reported as forced.
///    * unchanged (remote already at the source) → [`PushRefStatus::UpToDate`].
///    * otherwise [`PushRefStatus::Ok`].
/// 3. For accepted non-delete updates, copies the minimal object closure from the
///    LOCAL odb into the REMOTE odb via [`build_pack`] and
///    [`crate::index_pack::ingest_received_pack`], excluding objects already
///    reachable from the remote's existing ref tips.
/// 4. Applies the ref change on the remote (unless `opts.dry_run`).
///
/// When `opts.atomic` is set and any ref is rejected, no ref or object is
/// written and every otherwise-accepted ref is reported as
/// [`PushRefStatus::AtomicPushFailed`].
///
/// Both repositories must use the same object hash algorithm; the hash width is
/// threaded through [`Odb::hash_algo`] so SHA-256 repos work.
///
/// # Errors
///
/// Returns an error if either repository cannot be opened, if a source object is
/// missing from the local odb, or on I/O failure while writing objects or refs.
//
// TODO(phase: remote transports): `git://`, `http(s)`, and `ssh` push
// (receive-pack handshake + report-status parsing + credential helpers) are out
// of scope here and live in a later phase.
pub fn push_local(
    local_git_dir: &Path,
    remote_git_dir: &Path,
    refs: &[PushRefSpec],
    opts: &PushOptions,
) -> Result<PushOutcome> {
    if !remote_git_dir.join("objects").is_dir() {
        return Err(Error::Message(format!(
            "could not find repository at '{}'",
            remote_git_dir.display()
        )));
    }

    let local_odb = open_odb(local_git_dir);
    let remote_odb = open_odb(remote_git_dir);

    // Ancestry (fast-forward) checks run against the LOCAL repo, where the source
    // commits live. A remote-current oid that is not reachable from the source is
    // simply "not an ancestor", which is the correct non-fast-forward verdict.
    let local_repo = crate::repo::Repository::open(local_git_dir, &RepositoryOptions::empty()).ok();

    // The remote's existing ref tips become the `haves` for pack building, so the
    // copied object closure excludes everything the remote already has.
    let remote_have_tips: Vec<ObjectId> = crate::refs::list_refs(remote_git_dir, "refs/")?
        .into_iter()
        .map(|(_, oid)| oid)
        .collect();

    // First pass: decide each ref's status without mutating anything.
    let mut decisions: Vec<PushDecision> = Vec::with_capacity(refs.len());
    for spec in refs {
        decisions.push(decide_push(
            spec,
            &local_odb,
            remote_git_dir,
            local_repo.as_ref(),
        )?);
    }

    // Atomic: if any update would be rejected, apply none and demote the
    // otherwise-accepted updates to AtomicPushFailed.
    let any_rejected = decisions.iter().any(|d| d.result.status.is_error());
    if opts.atomic && any_rejected {
        for d in &mut decisions {
            if matches!(d.result.status, PushRefStatus::Ok) {
                d.result.status = PushRefStatus::AtomicPushFailed;
                d.apply = false;
            }
        }
        return Ok(PushOutcome {
            results: decisions.into_iter().map(|d| d.result).collect(),
        });
    }

    let push_wants: Vec<ObjectId> = decisions
        .iter()
        .filter(|d| d.apply && !opts.dry_run)
        .filter_map(|d| match &d.action {
            PushAction::Update(src) => Some(*src),
            _ => None,
        })
        .collect();
    if !push_wants.is_empty() && !opts.dry_run {
        let local_cfg = config_for_git_dir(local_git_dir);
        let pack_opts = if remote_have_tips.is_empty() {
            PackBuildOptions::for_local_copy(local_cfg.as_ref())
        } else {
            PackBuildOptions::for_local_push(local_cfg.as_ref())
        };
        let empty_shallow = HashSet::new();
        let remote_shallow = crate::shallow::load_shallow_boundaries(remote_git_dir);
        copy_objects_for_local_push(
            local_git_dir,
            remote_git_dir,
            &local_odb,
            &remote_odb,
            &push_wants,
            &remote_have_tips,
            &empty_shallow,
            &remote_shallow,
            &pack_opts,
        )?;
    }

    // Second pass: apply ref updates (one transaction per ref unless `atomic`).
    let mut ref_batch: Vec<crate::refs::store::RefUpdate> = Vec::new();
    for d in &mut decisions {
        if !d.apply || opts.dry_run {
            continue;
        }
        match &d.action {
            PushAction::Update(src) => {
                let update = push_store_ref_update(
                    d.result.remote_ref.as_str(),
                    Some(*src),
                    d.result.old_oid,
                );
                if opts.atomic {
                    ref_batch.push(update);
                } else if let Err(err) = crate::refs::commit_store_update(remote_git_dir, update) {
                    apply_ref_store_error_to_push_decision(d, &err, false);
                }
            }
            PushAction::Delete => {
                let update =
                    push_store_ref_update(d.result.remote_ref.as_str(), None, d.result.old_oid);
                if opts.atomic {
                    ref_batch.push(update);
                } else if let Err(err) = crate::refs::commit_store_update(remote_git_dir, update) {
                    apply_ref_store_error_to_push_decision(d, &err, false);
                }
            }
            PushAction::None => {}
        }
    }
    if opts.atomic && !opts.dry_run && !ref_batch.is_empty() {
        if let Err(err) = crate::refs::commit_ref_store_batch(remote_git_dir, &ref_batch) {
            apply_atomic_batch_ref_store_error(&mut decisions, &ref_batch, err);
        }
    }

    let results: Vec<_> = decisions.into_iter().map(|d| d.result).collect();
    if !opts.dry_run {
        if let Some(remote) = opts.tracking_remote.as_deref() {
            crate::branch_tracking::apply_push_remote_tracking_updates(
                local_git_dir,
                remote,
                &results,
            )?;
        }
    }

    Ok(PushOutcome { results })
}

fn push_store_ref_update(
    refname: &str,
    new_oid: Option<ObjectId>,
    expected_old: Option<ObjectId>,
) -> crate::refs::store::RefUpdate {
    crate::refs::store::RefUpdate {
        name: refname.to_owned(),
        new_value: new_oid.map(crate::refs::store::RawRef::Direct),
        expected: match expected_old {
            Some(oid) => crate::refs::store::Expected::Oid(oid),
            None => crate::refs::store::Expected::Any,
        },
        reflog: None,
        flags: Default::default(),
    }
}

fn push_status_for_ref_store_error(err: &Error) -> PushRefStatus {
    match err {
        Error::RefStore(crate::refs::store::RefStoreError::ExpectedMismatch { .. }) => {
            PushRefStatus::RejectStale
        }
        _ => PushRefStatus::RemoteRejected,
    }
}

fn ref_store_error_refname(err: &Error) -> Option<String> {
    let Error::RefStore(store_err) = err else {
        return None;
    };
    Some(match store_err {
        crate::refs::store::RefStoreError::ExpectedMismatch { name, .. }
        | crate::refs::store::RefStoreError::DuplicateUpdate { name }
        | crate::refs::store::RefStoreError::LockHeld { name }
        | crate::refs::store::RefStoreError::InvalidRefName { name }
        | crate::refs::store::RefStoreError::NotFound { name } => name.clone(),
        crate::refs::store::RefStoreError::NameUnavailable { reason } => match reason {
            crate::refs::RefnameUnavailable::SameBatch { refname, .. }
            | crate::refs::RefnameUnavailable::AncestorExists {
                new_ref: refname, ..
            }
            | crate::refs::RefnameUnavailable::DescendantExists {
                new_ref: refname, ..
            } => refname.clone(),
        },
        crate::refs::store::RefStoreError::SymrefLoop
        | crate::refs::store::RefStoreError::Corrupt(_) => return None,
    })
}

fn apply_ref_store_error_to_push_decision(d: &mut PushDecision, err: &Error, _atomic: bool) {
    d.result.status = push_status_for_ref_store_error(err);
    d.result.message = Some(err.to_string());
    d.apply = false;
}

fn apply_atomic_batch_ref_store_error(
    decisions: &mut [PushDecision],
    batch: &[crate::refs::store::RefUpdate],
    err: Error,
) {
    let fail_ref = ref_store_error_refname(&err);
    let message = err.to_string();
    let batch_names: HashSet<&str> = batch.iter().map(|u| u.name.as_str()).collect();
    let in_batch = decisions
        .iter_mut()
        .filter(|d| d.apply && batch_names.contains(d.result.remote_ref.as_str()));
    if let Some(fail_name) = fail_ref {
        for d in in_batch {
            if d.result.remote_ref == fail_name {
                d.result.status = push_status_for_ref_store_error(&err);
                d.result.message = Some(message.clone());
            } else {
                d.result.status = PushRefStatus::AtomicPushFailed;
                d.result.message = Some("atomic push failed".to_owned());
            }
            d.apply = false;
        }
    } else {
        let mut first = true;
        for d in in_batch {
            if first {
                d.result.status = push_status_for_ref_store_error(&err);
                d.result.message = Some(message.clone());
                first = false;
            } else {
                d.result.status = PushRefStatus::AtomicPushFailed;
                d.result.message = Some("atomic push failed".to_owned());
            }
            d.apply = false;
        }
    }
}

/// What a single accepted push update does once applied.
enum PushAction {
    /// Copy the closure of `src` to the remote and move the ref to `src`.
    Update(ObjectId),
    /// Delete the remote ref.
    Delete,
    /// No mutation (up-to-date or rejected).
    None,
}

/// A decided-but-not-yet-applied push update.
struct PushDecision {
    result: PushRefResult,
    action: PushAction,
    /// Whether the second pass should apply `action`.
    apply: bool,
}

/// Decide the status of a single [`PushRefSpec`] without mutating either repo.
fn decide_push(
    spec: &PushRefSpec,
    local_odb: &Odb,
    remote_git_dir: &Path,
    local_repo: Option<&crate::repo::Repository>,
) -> Result<PushDecision> {
    let remote_current = crate::refs::resolve_ref(remote_git_dir, &spec.dst).ok();

    // Up-to-date trumps every lease: pushing a non-delete to where the remote ref
    // already points is a no-op that succeeds even when the force-with-lease
    // expectation (a specific `expected_old` value, or `expect_absent`) does not
    // hold — "creating/moving a bookmark to the same place it already is is OK".
    // Must precede both the absence-lease and compare-and-swap checks below.
    if !spec.delete {
        if let Some(src) = spec.src {
            if remote_current == Some(src) {
                return Ok(PushDecision {
                    result: PushRefResult {
                        local_ref: None,
                        remote_ref: spec.dst.clone(),
                        old_oid: remote_current,
                        new_oid: Some(src),
                        forced: false,
                        deletion: false,
                        status: PushRefStatus::UpToDate,
                        message: None,
                    },
                    action: PushAction::None,
                    apply: false,
                });
            }
        }
    }

    // Absence lease (force-with-lease that the ref not exist): once the value is
    // actually changing (handled above), a destination that already exists fails
    // the lease and is rejected as stale.
    if spec.expect_absent && remote_current.is_some() {
        return Ok(PushDecision {
            result: PushRefResult {
                local_ref: None,
                remote_ref: spec.dst.clone(),
                old_oid: remote_current,
                new_oid: spec.src,
                forced: false,
                deletion: spec.delete,
                status: PushRefStatus::RejectStale,
                message: Some("stale info".to_owned()),
            },
            action: PushAction::None,
            apply: false,
        });
    }

    // Compare-and-swap (force-with-lease): the remote's current value must match
    // the caller's expectation, otherwise reject as stale. A `None` expectation
    // disables the value check.
    if let Some(expected) = spec.expected_old {
        if remote_current != Some(expected) {
            return Ok(PushDecision {
                result: PushRefResult {
                    local_ref: None,
                    remote_ref: spec.dst.clone(),
                    old_oid: remote_current,
                    new_oid: spec.src,
                    forced: false,
                    deletion: spec.delete,
                    status: PushRefStatus::RejectStale,
                    message: Some("stale info".to_owned()),
                },
                action: PushAction::None,
                apply: false,
            });
        }
    }

    if spec.delete {
        let (status, action, apply) = match remote_current {
            Some(_) => (PushRefStatus::Ok, PushAction::Delete, true),
            None => (PushRefStatus::UpToDate, PushAction::None, false),
        };
        return Ok(PushDecision {
            result: PushRefResult {
                local_ref: None,
                remote_ref: spec.dst.clone(),
                old_oid: remote_current,
                new_oid: None,
                forced: false,
                deletion: true,
                status,
                message: None,
            },
            action,
            apply,
        });
    }

    // Non-delete updates require a source object that exists locally.
    let Some(src) = spec.src else {
        return Err(Error::Message(format!(
            "push to '{}' has no source object and is not a deletion",
            spec.dst
        )));
    };
    if !local_odb.exists(&src) {
        return Err(Error::Message(format!(
            "source object {src} for '{}' is missing from the local object store",
            spec.dst
        )));
    }

    // Unchanged: the remote is already at the source.
    if remote_current == Some(src) {
        return Ok(PushDecision {
            result: PushRefResult {
                local_ref: None,
                remote_ref: spec.dst.clone(),
                old_oid: remote_current,
                new_oid: Some(src),
                forced: false,
                deletion: false,
                status: PushRefStatus::UpToDate,
                message: None,
            },
            action: PushAction::None,
            apply: false,
        });
    }

    // New ref: nothing on the remote yet — always allowed.
    let Some(old) = remote_current else {
        return Ok(PushDecision {
            result: PushRefResult {
                local_ref: None,
                remote_ref: spec.dst.clone(),
                old_oid: None,
                new_oid: Some(src),
                forced: false,
                deletion: false,
                status: PushRefStatus::Ok,
                message: None,
            },
            action: PushAction::Update(src),
            apply: true,
        });
    };

    // Existing ref: fast-forward when the remote's current commit is an ancestor
    // of the source. Otherwise it is a non-fast-forward update, allowed only with
    // force (reported as forced).
    let is_ff = local_repo
        .map(|r| crate::merge_base::is_ancestor(r, old, src).unwrap_or(false))
        .unwrap_or(false);

    if is_ff {
        Ok(PushDecision {
            result: PushRefResult {
                local_ref: None,
                remote_ref: spec.dst.clone(),
                old_oid: Some(old),
                new_oid: Some(src),
                forced: false,
                deletion: false,
                status: PushRefStatus::Ok,
                message: None,
            },
            action: PushAction::Update(src),
            apply: true,
        })
    } else if spec.force {
        Ok(PushDecision {
            result: PushRefResult {
                local_ref: None,
                remote_ref: spec.dst.clone(),
                old_oid: Some(old),
                new_oid: Some(src),
                forced: true,
                deletion: false,
                status: PushRefStatus::Ok,
                message: None,
            },
            action: PushAction::Update(src),
            apply: true,
        })
    } else {
        Ok(PushDecision {
            result: PushRefResult {
                local_ref: None,
                remote_ref: spec.dst.clone(),
                old_oid: Some(old),
                new_oid: Some(src),
                forced: false,
                deletion: false,
                status: PushRefStatus::RejectNonFastForward,
                message: Some("non-fast-forward".to_owned()),
            },
            action: PushAction::None,
            apply: false,
        })
    }
}

/// A remote ref selected for fetch, with its computed local destination.
pub(crate) struct MatchedRef {
    pub(crate) remote_ref: String,
    /// Destination local tracking ref, or `None` for an empty (no-store) dst.
    pub(crate) local_ref: Option<String>,
    pub(crate) oid: ObjectId,
    pub(crate) force: bool,
    pub(crate) is_tag: bool,
    /// Peeled target from ls-refs `peeled:` (annotated tags), when known.
    pub(crate) advertised_peel: Option<ObjectId>,
    /// How this ref appears in [`FETCH_HEAD`](https://git-scm.com/docs/git-fetch#_discussion).
    pub(crate) fetch_head: crate::fetch_request::FetchHeadPolicy,
}

/// Open an [`Odb`] for a git directory, attaching the git dir so `hash_algo`
/// (and MIDX config) resolve correctly.
pub(crate) fn open_odb(git_dir: &Path) -> Odb {
    Odb::new(&git_dir.join("objects")).with_config_git_dir(git_dir.to_path_buf())
}

fn config_for_git_dir(git_dir: &Path) -> Option<crate::config::ConfigSet> {
    crate::config::ConfigSet::load(
        &crate::environment::Environment::empty(),
        Some(git_dir),
        true,
    )
    .ok()
}

/// Write [`FETCH_HEAD`](https://git-scm.com/docs/git-fetch#_discussion) for anonymous URL/path fetches.
pub(crate) fn finish_anonymous_url_fetch(
    git_dir: &Path,
    opts: &FetchOptions,
    default_branch: Option<&str>,
    remote_refs: &[(String, ObjectId)],
    matched: &[MatchedRef],
) -> Result<()> {
    if opts.dry_run {
        return Ok(());
    }
    let write_fetch_head = opts.remote_name.is_none() || opts.explicit_refspec_fetch;
    if !write_fetch_head {
        return Ok(());
    }
    let Some(source) = opts.fetch_source.as_deref() else {
        return Ok(());
    };
    let lines = fetch_head_lines_for_ad_hoc_fetch(
        matched,
        remote_refs,
        &opts.refspecs,
        default_branch,
        source,
    );
    if lines.is_empty() {
        return Ok(());
    }
    crate::fetch_head::write_fetch_head(git_dir, &lines)
}

fn fetch_head_lines_for_ad_hoc_fetch(
    matched: &[MatchedRef],
    remote_refs: &[(String, ObjectId)],
    refspecs: &[String],
    default_branch: Option<&str>,
    source: &str,
) -> Vec<crate::fetch_head::FetchHeadLine> {
    use crate::fetch_head::FetchHeadLine;
    use crate::remote::Remote;

    let anonymous_only = refspecs.len() == 1
        && refspecs[0].trim_start_matches('+') == Remote::ANONYMOUS_FETCH_REFSPEC;

    if anonymous_only {
        if let Some(branch) = default_branch {
            let head_ref = format!("refs/heads/{branch}");
            if let Some(m) = matched
                .iter()
                .find(|m| m.remote_ref == head_ref && !m.is_tag)
            {
                return vec![FetchHeadLine {
                    oid: m.oid,
                    for_merge: true,
                    description: source.to_owned(),
                }];
            }
        }
        if let Some((_, oid)) = remote_refs.iter().find(|(name, _)| name == "HEAD") {
            return vec![FetchHeadLine {
                oid: *oid,
                for_merge: true,
                description: source.to_owned(),
            }];
        }
        return Vec::new();
    }

    matched
        .iter()
        .filter(|m| {
            m.fetch_head == crate::fetch_request::FetchHeadPolicy::ForMerge
                && !m.is_tag
                && m.remote_ref.starts_with("refs/heads/")
        })
        .map(|m| {
            let short = m
                .remote_ref
                .strip_prefix("refs/heads/")
                .unwrap_or(m.remote_ref.as_str());
            FetchHeadLine {
                oid: m.oid,
                for_merge: true,
                description: format!("branch '{short}' of {source}"),
            }
        })
        .collect()
}

/// Match a ref name against the positive refspecs, returning the destination
/// local ref name (`Some(name)`), `None`+stored=false collapsed: returns
/// `Some(Some(dst))` to store, `Some(None)` to fetch-without-store, or `None`
/// when no positive refspec matches.
pub(crate) fn match_positive(refname: &str, positive: &[RefspecItem]) -> Option<Option<String>> {
    for item in positive {
        let Some(src) = item.src.as_deref() else {
            continue;
        };
        if let Some(dst) = apply_refspec(src, item.dst.as_deref(), refname) {
            // Empty dst means "fetch but do not store".
            if dst.is_empty() {
                return Some(None);
            }
            return Some(Some(dst));
        }
    }
    None
}

/// Whether any positive refspec matching `refname` requested force (`+`).
pub(crate) fn refspecs_force(refname: &str, positive: &[RefspecItem]) -> bool {
    positive.iter().any(|item| {
        item.force
            && item
                .src
                .as_deref()
                .is_some_and(|src| apply_refspec(src, item.dst.as_deref(), refname).is_some())
    })
}

/// Whether `refname` is excluded by any negative refspec.
pub(crate) fn ref_excluded(refname: &str, negatives: &[RefspecItem]) -> bool {
    negatives.iter().any(|item| {
        item.src
            .as_deref()
            .is_some_and(|src| glob_matches(src, refname))
    })
}

/// Apply a `<src>[:<dst>]` refspec to `refname`, returning the destination ref.
///
/// Supports a single `*` wildcard (Git's fetch refspec form). When `dst` is
/// `None` the destination equals the matched source (rare for tracking fetches);
/// when `dst` is `Some("")` the empty string is returned (fetch-without-store).
pub(crate) fn apply_refspec(src: &str, dst: Option<&str>, refname: &str) -> Option<String> {
    match src.find('*') {
        Some(star) => {
            let prefix = &src[..star];
            let suffix = &src[star + 1..];
            if !refname.starts_with(prefix)
                || !refname.ends_with(suffix)
                || refname.len() < prefix.len() + suffix.len()
            {
                return None;
            }
            let middle = &refname[prefix.len()..refname.len() - suffix.len()];
            match dst {
                None => Some(refname.to_owned()),
                Some("") => Some(String::new()),
                Some(d) => Some(d.replacen('*', middle, 1)),
            }
        }
        None => {
            if src != refname {
                return None;
            }
            match dst {
                None => Some(refname.to_owned()),
                Some("") => Some(String::new()),
                Some(d) => Some(d.to_owned()),
            }
        }
    }
}

/// Whether `pattern` (a refspec src side, possibly with one `*`) matches `refname`.
fn glob_matches(pattern: &str, refname: &str) -> bool {
    match pattern.find('*') {
        Some(star) => {
            let prefix = &pattern[..star];
            let suffix = &pattern[star + 1..];
            refname.starts_with(prefix)
                && refname.ends_with(suffix)
                && refname.len() >= prefix.len() + suffix.len()
        }
        None => pattern == refname,
    }
}

/// Add tags to the matched set according to [`TagMode`].
///
/// Under `Following`, tags named in `already_local` (see
/// [`crate::fetch::tags_already_local`]) are skipped, matching Git's auto-follow,
/// which never reconsiders a tag the local repository already has.
#[allow(clippy::too_many_arguments)]
pub(crate) fn apply_tag_mode(
    mode: TagMode,
    remote_refs: &[(String, ObjectId)],
    remote_odb: &Odb,
    shallow_boundaries: &HashSet<ObjectId>,
    negatives: &[RefspecItem],
    already_local: &HashSet<String>,
    matched: &mut Vec<MatchedRef>,
    matched_oids: &mut HashSet<ObjectId>,
    seen_remote_ref: &mut HashSet<String>,
) -> Result<HashSet<ObjectId>> {
    let mut following_only = HashSet::new();
    if mode == TagMode::None {
        return Ok(following_only);
    }

    // Tags this call may add, with the object each one peels to (`None` when
    // the tag cannot be peeled; only the tag oid itself can then match).
    let candidates: Vec<(&String, ObjectId, Option<ObjectId>)> = remote_refs
        .iter()
        .filter(|(name, _)| {
            name.starts_with("refs/tags/")
                && !seen_remote_ref.contains(name)
                && !ref_excluded(name, negatives)
                && !(mode == TagMode::Following && already_local.contains(name))
        })
        .map(|(name, oid)| {
            let peeled = match mode {
                TagMode::Following => remote_odb.peel_tags(*oid).ok().map(|(p, _)| p),
                _ => None,
            };
            (name, *oid, peeled)
        })
        .collect();

    // For Following, decide reachability on the source ODB using both the local
    // and remote shallow grafts (unioned by the caller). Post-pack pruning via
    // [`crate::fetch::retain_following_tags`] still drops tags whose objects did
    // not arrive. Using the local ODB here would miss every tip on first fetch.
    let following_closure: HashSet<ObjectId> = if mode == TagMode::Following {
        let roots: Vec<ObjectId> = matched
            .iter()
            .filter(|m| !m.is_tag)
            .map(|m| m.oid)
            .collect();
        let targets: HashSet<ObjectId> = candidates
            .iter()
            .map(|(_, oid, peeled)| peeled.unwrap_or(*oid))
            .collect();
        crate::fetch::reachable_commits(remote_odb, &roots, &targets, shallow_boundaries)
    } else {
        HashSet::new()
    };

    for (name, oid, peeled) in candidates {
        let keep = match mode {
            TagMode::All => true,
            // Keep when the tag (or what it peels to) is in the fetched closure.
            TagMode::Following => {
                following_closure.contains(&oid)
                    || peeled.is_some_and(|p| following_closure.contains(&p))
            }
            TagMode::None => false,
        };
        if keep && seen_remote_ref.insert(name.clone()) {
            if matched_oids.insert(oid) && mode == TagMode::Following {
                following_only.insert(oid);
            }
            matched.push(MatchedRef {
                remote_ref: name.clone(),
                local_ref: Some(name.clone()),
                oid,
                force: false,
                is_tag: true,
                advertised_peel: peeled,
                fetch_head: crate::fetch_request::FetchHeadPolicy::ForMerge,
            });
        }
    }
    Ok(following_only)
}

/// Whether a tag ref's peeled target (or the oid itself for lightweight tags)
/// exists in the local or remote object database.
fn ref_target_exists(local_odb: &Odb, remote_odb: &Odb, tag_oid: ObjectId) -> bool {
    let peel_odb = if local_odb.exists(&tag_oid) {
        local_odb
    } else {
        remote_odb
    };
    peel_odb
        .peel_tags(tag_oid)
        .is_ok_and(|(peeled, _)| local_odb.exists(&peeled))
}

/// Classify a single ref update into an [`UpdateMode`].
pub(crate) fn classify_update(
    old: Option<&ObjectId>,
    new: &ObjectId,
    force: bool,
    is_tag: bool,
    repo: Option<&crate::repo::Repository>,
) -> UpdateMode {
    let Some(old) = old else {
        return UpdateMode::New;
    };
    if old == new {
        return UpdateMode::UpToDate;
    }
    // Fast-forward when old is an ancestor of new (commit history only).
    let ff = repo
        .map(|r| crate::merge_base::is_ancestor(r, *old, *new).unwrap_or(false))
        .unwrap_or(false);
    if ff && !is_tag {
        return UpdateMode::FastForward;
    }
    if force {
        return UpdateMode::Forced;
    }
    if is_tag {
        return UpdateMode::TagUpdateRejected;
    }
    UpdateMode::NonFastForwardRejected
}

/// Default-branch symref for a wildcard refspec destination (`<prefix>HEAD`).
///
/// Remote names may contain `/` (e.g. `team/origin` → `refs/remotes/team/origin/HEAD`).
fn is_default_head_under_tracking_prefix(refname: &str, tracking_prefix: &str) -> bool {
    refname == format!("{tracking_prefix}HEAD")
}

/// Delete local tracking refs whose remote counterpart no longer exists.
///
/// A local tracking ref is a prune candidate when it lives under the destination
/// namespace of some positive wildcard refspec and no current remote ref maps to
/// it. Matches `git fetch --prune` for the common `refs/remotes/<remote>/*` case.
pub(crate) fn prune_tracking_refs(
    local_git_dir: &Path,
    positive: &[RefspecItem],
    remote_refs: &[(String, ObjectId)],
    dry_run: bool,
    updates: &mut Vec<RefUpdate>,
    mut store_batch: Option<&mut Vec<crate::refs::store::RefUpdate>>,
) -> Result<()> {
    // Set of local tracking refs that the current remote justifies.
    let mut live: HashSet<String> = HashSet::new();
    for (name, _) in remote_refs {
        if let Some(Some(dst)) = match_positive(name, positive) {
            live.insert(dst);
        }
    }

    let mut pruned: HashMap<String, ObjectId> = HashMap::new();
    for item in positive {
        let Some(dst) = item.dst.as_deref() else {
            continue;
        };
        if let Some(star) = dst.find('*') {
            // Wildcard refspec: enumerate existing local refs under its
            // destination prefix and prune those the current remote no longer
            // justifies.
            let prefix = &dst[..star];
            for (name, oid) in crate::refs::list_refs(local_git_dir, prefix)? {
                if !name.starts_with(prefix) {
                    continue;
                }
                if is_default_head_under_tracking_prefix(&name, prefix) {
                    continue;
                }
                if !live.contains(&name) {
                    pruned.entry(name).or_insert(oid);
                }
            }
        } else if !live.contains(dst) {
            let exact_prefix = dst.strip_suffix("HEAD").filter(|p| p.ends_with('/'));
            if exact_prefix.is_some_and(|p| is_default_head_under_tracking_prefix(dst, p)) {
                continue;
            }
            // Exact refspec (e.g. `refs/heads/a2:refs/remotes/origin/a2`): when
            // the source ref is gone from the remote, `dst` is absent from `live`,
            // so prune the tracking ref if it still exists locally. This is the
            // explicit `git fetch <remote> <branch>` / `--prune` deletion case.
            if let Ok(oid) = crate::refs::resolve_ref(local_git_dir, dst) {
                pruned.entry(dst.to_owned()).or_insert(oid);
            }
        }
    }

    let prune_delete_flags = crate::refs::store::RefUpdateFlags {
        no_deref: true,
        ..Default::default()
    };
    for (name, oid) in pruned {
        if !dry_run {
            if let Some(batch) = store_batch.as_mut() {
                batch.push(crate::refs::store::RefUpdate {
                    name: name.clone(),
                    new_value: None,
                    expected: crate::refs::store::Expected::Any,
                    reflog: None,
                    flags: prune_delete_flags,
                });
            } else {
                crate::refs::delete_ref(local_git_dir, &name)?;
            }
        }
        updates.push(RefUpdate {
            remote_ref: String::new(),
            local_ref: Some(name),
            old_oid: Some(oid),
            new_oid: None,
            mode: UpdateMode::DeletedMissing,
            note: Some("pruned (gone on remote)".to_owned()),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::progress::ProgressPhase;
    use crate::ref_storage::RefStorageFormat;
    use crate::repo::init_repository;
    use std::process::Command;

    #[derive(Debug, Default)]
    struct PhaseRecorder {
        events: Vec<&'static str>,
        started: Vec<(ProgressPhase, Option<u64>)>,
        finished: Vec<ProgressPhase>,
        counts: Vec<u64>,
        active: Option<ProgressPhase>,
        current: u64,
    }

    impl ProgressSink for PhaseRecorder {
        fn start(&mut self, phase: ProgressPhase, total: Option<u64>) {
            self.events.push("start");
            self.started.push((phase, total));
            self.active = Some(phase);
            self.current = 0;
        }
        fn inc(&mut self, units: u64) {
            self.events.push("inc");
            self.current += units;
            self.counts.push(self.current);
        }
        fn set(&mut self, current: u64) {
            self.events.push("set");
            self.current = current;
            self.counts.push(self.current);
        }
        fn finish(&mut self) {
            self.events.push("finish");
            if let Some(phase) = self.active.take() {
                self.finished.push(phase);
            }
        }
    }

    impl Progress for PhaseRecorder {}

    fn git_ok(dir: &Path, args: &[&str]) -> bool {
        Command::new("git") // hygiene: git fixture comparison in cfg(test) module
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.com")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.com")
            .current_dir(dir)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    fn assert_monotonic(counts: &[u64]) {
        for pair in counts.windows(2) {
            assert!(pair[1] >= pair[0], "progress counts must be monotonic");
        }
    }

    #[test]
    fn fetch_local_reports_copy_progress() {
        let remote = tempfile::tempdir().expect("remote");
        assert!(git_ok(
            remote.path(),
            &["init", "-q", "-b", "main", "--bare"]
        ));
        let src = tempfile::tempdir().expect("src");
        assert!(git_ok(src.path(), &["init", "-q", "-b", "main"]));
        std::fs::write(src.path().join("f.txt"), "payload\n").unwrap();
        assert!(git_ok(src.path(), &["add", "f.txt"]));
        assert!(git_ok(src.path(), &["commit", "-qm", "c1"]));
        assert!(git_ok(
            src.path(),
            &["push", "-q", remote.path().to_str().unwrap(), "main:main",],
        ));

        let local = tempfile::tempdir().expect("local");
        init_repository(local.path(), false, "main", None, RefStorageFormat::Files).expect("init");

        let mut opts = FetchOptions {
            refspecs: vec!["+refs/heads/*:refs/remotes/origin/*".to_owned()],
            initial_remote_fetch: true,
            tags: TagMode::None,
            ..Default::default()
        };
        let mut rec = PhaseRecorder::default();
        fetch_local_with_progress(
            &local.path().join(".git"),
            remote.path(),
            &mut opts,
            &mut rec,
        )
        .expect("fetch_local");

        assert!(
            rec.started
                .iter()
                .any(|(p, _)| *p == ProgressPhase::CopyingObjects),
            "expected CopyingObjects phase"
        );
        assert!(
            rec.finished.contains(&ProgressPhase::CopyingObjects),
            "expected CopyingObjects finish"
        );
        assert!(!rec.counts.is_empty(), "expected copy progress counts");
        assert_monotonic(&rec.counts);
        let first_progress = rec.events.iter().find(|e| **e != "start").copied();
        assert_eq!(
            rec.events.first().copied(),
            Some("start"),
            "CopyingObjects must start before link/copy work reports progress"
        );
        assert!(
            first_progress.is_some(),
            "expected set/inc after start during copy"
        );
    }

    #[test]
    fn transfer_stats_from_pack_dir_reads_index_without_verify() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        assert!(git_ok(&repo, &["init", "-q", "-b", "main", "."]));
        std::fs::write(repo.join("blob.txt"), b"payload\n").unwrap();
        assert!(git_ok(&repo, &["add", "blob.txt"]));
        assert!(git_ok(&repo, &["commit", "-qm", "c"]));
        assert!(git_ok(&repo, &["repack", "-a", "-d", "-f"]));

        let pack_dir = repo.join(".git/objects/pack");
        let stats = transfer_stats_from_pack_dir(&pack_dir).expect("stats");
        assert!(stats.bytes > 0);
        assert!(stats.objects > 0);

        let pack_path = std::fs::read_dir(&pack_dir)
            .expect("read pack dir")
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .find(|p| p.extension().is_some_and(|e| e == "pack"))
            .expect("pack file");
        let idx = pack_path.with_extension("idx");
        let index = crate::pack::read_pack_index_no_verify(&idx).expect("idx");
        assert_eq!(
            stats.objects,
            u64::try_from(index.len()).expect("count"),
            "object count must match the on-disk index entry count"
        );
    }

    #[test]
    fn update_mode_modified_repo_excludes_rejected_tag_updates() {
        assert!(UpdateMode::New.modified_repo());
        assert!(UpdateMode::DeletedMissing.modified_repo());
        assert!(!UpdateMode::TagUpdateRejected.modified_repo());
        assert!(!UpdateMode::UpToDate.modified_repo());
    }
}
