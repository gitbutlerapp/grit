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
use crate::partial_clone::ObjectFilterSpec;

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::error::{Error, RemoteUnreadableReason, Result};
use crate::fetch::{NoProgress, Progress};
use crate::objects::{parse_commit, parse_tag, parse_tree, HashAlgo, ObjectId, ObjectKind};
use crate::odb::Odb;
use crate::progress::{ProgressPhase, ProgressSink, ReceivedObjects};
use crate::push_report::{PushRefResult, PushRefStatus};
use crate::refspec::{parse_fetch_refspec, RefspecItem};
use std::num::NonZeroU32;

/// How a single reference resolved during a fetch (or would resolve in a push).
///
/// Mirrors the shapes of `gix::remote::fetch::refs::update::Mode` that `jj`
/// already consumes, so the embedder's translation layer stays a thin adapter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde-models", derive(serde::Serialize))]
#[cfg_attr(feature = "serde-models", serde(rename_all = "snake_case"))]
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
    /// The local tracking ref could not be created or updated (namespace conflict, lock, …).
    RefLockRejected,
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

    /// Returns whether fetch refused this update (non-fast-forward without force, a moved
    /// tag, or a source missing on the remote).
    #[must_use]
    pub fn is_rejected(self) -> bool {
        matches!(
            self,
            Self::NonFastForwardRejected
                | Self::TagUpdateRejected
                | Self::SourceObjectNotFound
                | Self::RefLockRejected
        )
    }

    /// Returns whether this update belongs in a fetch report: the ref changed
    /// ([`Self::modified_repo`]) or the update was refused ([`Self::is_rejected`]).
    ///
    /// Up-to-date, no-op and unborn refs are left out.
    #[must_use]
    pub fn is_reported(self) -> bool {
        self.modified_repo() || self.is_rejected()
    }
}

/// The resolved outcome of one reference during a fetch.
#[derive(Clone, Debug)]
#[cfg_attr(feature = "serde-models", derive(serde::Serialize))]
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
    /// Fetch tags that point into the history being fetched (like `git fetch`'s
    /// default tag-following; not what `git clone` does).
    ///
    /// A tag is kept when the object it peels to arrives with this fetch, when
    /// it is a commit already present locally and reachable from the fetched
    /// refs, or when it is a tree or blob already present locally. Annotated
    /// tags also need their tag object. Deepening or unshallowing follows tags
    /// into the newly available history in the same fetch, without letting
    /// automatic tags expand the requested branch depth.
    ///
    /// Git also keeps a tag on a local commit
    /// that the fetched refs do not reach; grit does not. Deciding this reads
    /// no objects for delivered targets, walks only commits for local ones, and
    /// never lazy-fetches in a partial clone, so it stays cheap on large
    /// histories.
    ///
    /// Tags the local repository already has at the advertised value are not
    /// considered again, so they do not appear in [`FetchOutcome::updates`].
    #[default]
    Following,
    /// Fetch every advertised `refs/tags/*` ref (same idea as `git clone` and
    /// `git fetch --tags`).
    All,
}

/// Shallow-history control for fetch and clone (`--depth`, `--deepen`, `--unshallow`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FetchDepth {
    /// Truncate or establish history at the given absolute depth from each tip
    /// (`git fetch --depth N`).
    Depth(NonZeroU32),
    /// Deepen an existing shallow repository by the given number of commits
    /// (`git fetch --deepen N`, wire `deepen N` + `deepen-relative`).
    Deepen(NonZeroU32),
    /// Convert a shallow repository into a complete one (`git fetch --unshallow`).
    Unshallow,
}

impl FetchDepth {
    /// Build a [`FetchDepth::Depth`] when `n > 0`.
    #[must_use]
    pub fn depth(n: u32) -> Option<Self> {
        NonZeroU32::new(n).map(Self::Depth)
    }

    /// Build a [`FetchDepth::Deepen`] when `n > 0`.
    #[must_use]
    pub fn deepen(n: u32) -> Option<Self> {
        NonZeroU32::new(n).map(Self::Deepen)
    }
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
    /// Shallow fetch control (`--depth`, `--deepen`, `--unshallow`). `None` leaves
    /// history depth unchanged (aside from an existing on-disk `shallow` file).
    pub fetch_depth: Option<FetchDepth>,
    /// Deepen history to include commits no older than this cutoff
    /// (`git fetch --shallow-since <date>`). The value is sent verbatim as the
    /// wire `deepen-since <value>`; callers should pass the Unix timestamp Git's
    /// `upload-pack` expects (a bare integer), not a human date string.
    pub deepen_since: Option<String>,
    /// Deepen history but stop at (exclude) commits reachable from these refs/oids
    /// (`git fetch --shallow-exclude <ref>`). Each entry is sent as a wire
    /// `deepen-not <ref>`.
    pub deepen_not: Vec<String>,
    /// First fetch populating a remote-tracking namespace (clone): write `packed-refs`
    /// and `refs/remotes/{remote}/HEAD`.
    pub initial_remote_fetch: bool,
    /// Remote name paired with [`Self::initial_remote_fetch`] (e.g. `"origin"`).
    pub remote_name: Option<String>,
    /// Source label written into [`FETCH_HEAD`](https://git-scm.com/docs/git-fetch#_discussion).
    /// [`crate::remote::Remote::fetch`] sets this from the fetch URL when unset.
    pub fetch_source: Option<String>,
    /// Configured `remote.<name>.fetch` refspecs (for opportunistic tracking when [`Self::refspecs`]
    /// overrides them). [`crate::remote::Remote::fetch`] fills this for named remotes.
    pub configured_refspecs: Vec<String>,
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
    /// Object filter for partial clone (`blob:none`, `blob:limit=<n>`). Omitted when
    /// unset; promisor remotes may reapply `remote.*.partialclonefilter` automatically.
    pub filter: Option<ObjectFilterSpec>,
    /// Pick the filter from the promisor remotes the server advertises and the client
    /// accepts, from their advertised `partialCloneFilter` (Git's `--filter=auto`; see
    /// [`PromisorDecision::combined_filter`](crate::promisor_remote::PromisorDecision::combined_filter)).
    /// Ignored when [`Self::filter`] is set. A remote whose stored filter is `auto` turns it on.
    /// Without an accepted remote that advertises a usable filter, the fetch is unfiltered.
    pub auto_filter: bool,
    /// When true, the received pack is written with a sibling `.promisor` marker.
    ///
    /// Set by the fetch driver when a filter was sent on the wire; callers should
    /// leave this `false`.
    pub mark_received_pack_promisor: bool,
    /// Explicit object ids to fetch (promisor on-demand); skips refspec matching and ref updates.
    pub object_wants: Vec<ObjectId>,
    /// On-demand promisor object fetch: always mark the received pack promisor.
    pub promisor_object_fetch: bool,
    /// Initial fetch from a plain local path: hard-link the source's packfiles and
    /// loose objects wholesale instead of walking the requested refs (Git's
    /// `clone --local`).
    ///
    /// The destination then also receives objects unreachable from the fetched
    /// refs (other refs, excluded tags, unreachable leftovers). Only honored for
    /// a local transport with no local history, no shallow request and a source
    /// without alternates; otherwise the fetch sends exactly the objects
    /// reachable from the requested refs. [`crate::remote::clone::clone`] sets it
    /// for plain-path sources (not `file://` URLs). Defaults to `false`.
    pub link_source_objects: bool,
}

impl Default for FetchOptions {
    fn default() -> Self {
        Self {
            refspecs: Vec::new(),
            negative_refspecs: Vec::new(),
            tags: TagMode::Following,
            prune: false,
            dry_run: false,
            fetch_depth: None,
            deepen_since: None,
            deepen_not: Vec::new(),
            initial_remote_fetch: false,
            remote_name: None,
            fetch_source: None,
            configured_refspecs: Vec::new(),
            clone_reflog: None,
            diagnostics: None,
            network_trace: false,
            index_pack_threads: None,
            thread_policy: crate::parallel::ThreadPolicy::default(),
            environment: crate::environment::Environment::empty(),
            pack_spool_only: false,
            http_protocol_version: None,
            remote_progress: false,
            filter: None,
            auto_filter: false,
            mark_received_pack_promisor: false,
            object_wants: Vec::new(),
            promisor_object_fetch: false,
            link_source_objects: false,
        }
    }
}

/// Identity and message for clone reflog entries written by the library.
#[derive(Clone, Debug)]
pub struct CloneReflog {
    /// Committer identity for the clone reflog line.
    pub identity: crate::ident::Signature,
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
            fetch_depth: self.fetch_depth,
            deepen_since: self.deepen_since.clone(),
            deepen_not: self.deepen_not.clone(),
            initial_remote_fetch: self.initial_remote_fetch,
            remote_name: self.remote_name.clone(),
            fetch_source: self.fetch_source.clone(),
            configured_refspecs: self.configured_refspecs.clone(),
            clone_reflog: self.clone_reflog.clone(),
            diagnostics: self.diagnostics.clone(),
            network_trace: self.network_trace,
            index_pack_threads: self.index_pack_threads,
            thread_policy: self.thread_policy,
            environment: self.environment.clone(),
            pack_spool_only: self.pack_spool_only,
            http_protocol_version: self.http_protocol_version,
            remote_progress: self.remote_progress,
            filter: self.filter,
            auto_filter: self.auto_filter,
            mark_received_pack_promisor: self.mark_received_pack_promisor,
            object_wants: self.object_wants.clone(),
            promisor_object_fetch: self.promisor_object_fetch,
            link_source_objects: self.link_source_objects,
        }
    }
}

impl FetchOptions {
    /// Whether this fetch carries any shallow/deepen request (an explicit
    /// `fetch_depth`/`deepen-since`/`deepen-not`). Note this does NOT cover the
    /// "already shallow, fetching more of the same boundary" case — that is
    /// driven by the on-disk `shallow` file, checked separately by the fetch
    /// paths via [`crate::shallow::load_shallow_oids`].
    #[must_use]
    pub fn has_deepen_request(&self) -> bool {
        self.fetch_depth.is_some()
            || self
                .deepen_since
                .as_deref()
                .is_some_and(|v| !v.trim().is_empty())
            || self.deepen_not.iter().any(|v| !v.trim().is_empty())
    }

    /// Wire `deepen N` value and whether `deepen-relative` must follow.
    #[must_use]
    pub(crate) fn wire_deepen(&self) -> Option<(u32, bool)> {
        match self.fetch_depth {
            Some(FetchDepth::Depth(n)) => Some((n.get(), false)),
            Some(FetchDepth::Deepen(n)) => Some((n.get(), true)),
            Some(FetchDepth::Unshallow) => Some((crate::shallow::INFINITE_DEPTH, false)),
            None => None,
        }
    }

    /// Absolute depth hint for tag-following on a shallow fetch (`--depth` only).
    #[must_use]
    pub(crate) fn absolute_depth_for_tags(&self) -> Option<u32> {
        match self.fetch_depth {
            Some(FetchDepth::Depth(n)) => Some(n.get()),
            Some(FetchDepth::Deepen(_)) | Some(FetchDepth::Unshallow) | None => None,
        }
    }
}

/// Validate shallow fetch options against the local repository before negotiation.
///
/// # Errors
///
/// Returns [`Error::FetchUnshallowNotShallow`] when [`FetchDepth::Unshallow`] is
/// requested but the repository has no shallow boundaries.
pub fn validate_fetch_depth_local(local_git_dir: &Path, opts: &FetchOptions) -> Result<()> {
    if matches!(opts.fetch_depth, Some(FetchDepth::Unshallow)) {
        let local = crate::shallow::load_shallow_oids(local_git_dir)?;
        if local.is_empty() {
            return Err(Error::FetchUnshallowNotShallow);
        }
    }
    Ok(())
}

/// Require server shallow/deepen capabilities for the requested fetch options.
pub(crate) fn validate_fetch_depth_server(
    opts: &FetchOptions,
    v0_caps: Option<&str>,
    v2_caps: Option<&[String]>,
) -> Result<()> {
    let needs_shallow = opts.has_deepen_request()
        || opts.wire_deepen().is_some()
        || opts
            .deepen_since
            .as_deref()
            .is_some_and(|s| !s.trim().is_empty())
        || opts.deepen_not.iter().any(|s| !s.trim().is_empty());
    if !needs_shallow {
        return Ok(());
    }
    if let Some(caps) = v0_caps {
        if !caps.contains("shallow") {
            return Err(Error::FetchShallowUnsupported);
        }
        if opts.wire_deepen().is_some_and(|(_, rel)| rel) && !caps.contains("deepen-relative") {
            return Err(Error::FetchDeepenRelativeUnsupported);
        }
    } else if let Some(caps) = v2_caps {
        if !crate::protocol_v2::fetch_supports_feature(caps, "shallow") {
            return Err(Error::FetchShallowUnsupported);
        }
    }
    Ok(())
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

/// Resolve tag-following policy for a fetch.
///
/// When `cli_tags` is [`Some`], `true` maps to [`TagMode::All`] (`--tags`) and
/// `false` to [`TagMode::None`] (`--no-tags`). Otherwise reads
/// `remote.<remote>.tagOpt` (`--tags` / `--no-tags`, Git-compatible) and defaults
/// to [`TagMode::Following`].
#[must_use]
pub fn resolve_fetch_tags(
    config: &crate::config::ConfigSet,
    remote: &str,
    cli_tags: Option<bool>,
) -> TagMode {
    if let Some(all) = cli_tags {
        return if all { TagMode::All } else { TagMode::None };
    }
    let key = format!("remote.{remote}.tagOpt");
    match config.get(&key).as_deref().map(str::trim) {
        Some("--tags") | Some("tags") | Some("all") => TagMode::All,
        Some("--no-tags") | Some("no-tags") | Some("no") | Some("never") => TagMode::None,
        _ => TagMode::Following,
    }
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
#[cfg_attr(feature = "serde-models", derive(serde::Serialize))]
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
    /// Build stats from an ingested pack (installed under `objects/pack/` or written
    /// into a custom primary store).
    #[must_use]
    pub fn from_ingested_pack(ingested: &crate::index_pack::IngestedPack) -> Self {
        Self {
            objects: u64::try_from(ingested.object_ids.len()).unwrap_or(u64::MAX),
            bytes: ingested.pack_bytes,
            kinds: Some(ingested.object_counts),
        }
    }
}

/// Totals across several transfers, such as every remote of a fetch-all.
///
/// Counts and bytes add up. [`TransferStats::kinds`] stays known only while every
/// transfer that received objects reported its type breakdown.
impl std::ops::AddAssign for TransferStats {
    fn add_assign(&mut self, other: Self) {
        self.kinds = match (self.kinds, other.kinds) {
            (Some(mut mine), Some(theirs)) => {
                mine += theirs;
                Some(mine)
            }
            (mine, None) if other.objects == 0 => mine,
            (None, theirs) if self.objects == 0 => theirs,
            _ => None,
        };
        self.objects += other.objects;
        self.bytes += other.bytes;
    }
}

/// The structured result of a fetch, ready for the embedder's ref-store apply.
#[derive(Clone, Debug, Default)]
#[cfg_attr(feature = "serde-models", derive(serde::Serialize))]
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
    /// True when an object filter from [`FetchOptions::filter`] was sent on the wire
    /// and the server advertised filter support. False with a requested filter means
    /// the server does not support `filter` and sent every object; as in Git, the pack
    /// is still marked promisor and the remote stays a promisor remote.
    pub filter_sent: bool,
    /// The `promisor-remote` exchange, when the server advertised promisor remotes
    /// (protocol v2 only). Says which remotes were accepted and why others were not.
    pub promisor: Option<crate::promisor_remote::PromisorHandshake>,
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

    /// Updates worth reporting to a user, in fetch order ([`UpdateMode::is_reported`]).
    pub fn reported_updates(&self) -> impl Iterator<Item = &RefUpdate> {
        self.updates.iter().filter(|u| u.mode.is_reported())
    }

    /// Number of local refs this fetch changed ([`UpdateMode::modified_repo`]).
    ///
    /// For a [`FetchOptions::dry_run`] fetch this counts the refs that *would* change;
    /// nothing was written.
    #[must_use]
    pub fn changed_ref_count(&self) -> usize {
        self.updates
            .iter()
            .filter(|u| u.mode.modified_repo())
            .count()
    }

    /// Whether any ref update was refused ([`UpdateMode::is_rejected`]).
    #[must_use]
    pub fn has_rejections(&self) -> bool {
        self.updates.iter().any(|u| u.mode.is_rejected())
    }
}

fn object_kind_counts_for_oids(
    odb: &Odb,
    oids: &HashSet<ObjectId>,
) -> Result<crate::index_pack::ObjectKindCounts> {
    use crate::index_pack::ObjectKindCounts;
    let mut kinds = ObjectKindCounts::default();
    for oid in oids {
        kinds.add(odb.read_info(oid)?.kind);
    }
    Ok(kinds)
}

fn loose_transfer_stats(odb: &Odb, oids: &HashSet<ObjectId>) -> (u64, u64) {
    let mut objects = 0u64;
    let mut bytes = 0u64;
    for size in oids.iter().filter_map(|oid| odb.local_loose_file_size(oid)) {
        objects += 1;
        bytes += size;
    }
    (objects, bytes)
}

fn pack_file_basenames(pack_dir: &Path) -> HashSet<String> {
    let mut names = HashSet::new();
    let Ok(entries) = fs::read_dir(pack_dir) else {
        return names;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "pack") {
            if let Some(name) = path.file_name() {
                names.insert(name.to_string_lossy().into_owned());
            }
        }
    }
    names
}

const LOCAL_LINK_KINDS_OID_PROBE_MAX: u64 = 4096;

fn oids_from_newly_linked_pack_indexes(
    local_odb: &Odb,
    pack_dir_before: &HashSet<String>,
) -> Result<Vec<ObjectId>> {
    let pack_dir = local_odb.objects_dir().join("pack");
    let mut oids = Vec::new();
    if !pack_dir.is_dir() {
        return Ok(oids);
    }
    for entry in fs::read_dir(&pack_dir).map_err(Error::Io)? {
        let path = entry.map_err(Error::Io)?.path();
        if path.extension().is_none_or(|e| e != "pack") {
            continue;
        }
        let Some(name) = path.file_name() else {
            continue;
        };
        if pack_dir_before.contains(name.to_string_lossy().as_ref()) {
            continue;
        }
        let idx = path.with_extension("idx");
        if !idx.is_file() {
            continue;
        }
        let index = crate::pack::read_pack_index_no_verify(&idx)?;
        for entry in index.iter() {
            let oid = ObjectId::from_bytes(entry.oid())?;
            oids.push(oid);
        }
    }
    Ok(oids)
}

fn enrich_local_link_transfer_kinds(
    local_odb: &Odb,
    pack_dir_before: &HashSet<String>,
    loose_reachable: &HashSet<ObjectId>,
    transfer: &mut TransferStats,
) -> Result<()> {
    if transfer.kinds.is_some() || transfer.objects == 0 {
        return Ok(());
    }
    if transfer.objects > LOCAL_LINK_KINDS_OID_PROBE_MAX {
        return Ok(());
    }
    let mut probe: HashSet<ObjectId> = loose_reachable.clone();
    for oid in oids_from_newly_linked_pack_indexes(local_odb, pack_dir_before)? {
        probe.insert(oid);
    }
    if u64::try_from(probe.len()).unwrap_or(u64::MAX) != transfer.objects {
        return Ok(());
    }
    transfer.kinds = Some(object_kind_counts_for_oids(local_odb, &probe)?);
    Ok(())
}

/// Per-fetch transfer stats for objects installed during one local copy operation.
///
/// `loose_candidates` limits which installed objects are looked for as loose files (each
/// costs a `stat`); `None` checks all of `installed`.
fn transfer_stats_for_local_operation(
    local_odb: &Odb,
    installed: &HashSet<ObjectId>,
    loose_candidates: Option<&HashSet<ObjectId>>,
    pack_dir_before: &HashSet<String>,
) -> Result<TransferStats> {
    let objects_dir = local_odb.objects_dir();
    let pack_dir = objects_dir.join("pack");
    let mut bytes = 0u64;
    let mut objects_from_new_packs = 0u64;
    if pack_dir.is_dir() {
        for entry in fs::read_dir(&pack_dir).map_err(Error::Io)? {
            let path = entry.map_err(Error::Io)?.path();
            if path.extension().is_none_or(|e| e != "pack") {
                continue;
            }
            let Some(name) = path.file_name() else {
                continue;
            };
            let name = name.to_string_lossy();
            if pack_dir_before.contains(name.as_ref()) {
                continue;
            }
            bytes += fs::metadata(&path).map_err(Error::Io)?.len();
            let idx = path.with_extension("idx");
            if idx.is_file() {
                let index = crate::pack::read_pack_index_no_verify(&idx)?;
                objects_from_new_packs += u64::try_from(index.len()).unwrap_or(u64::MAX);
            }
        }
    }
    let (loose_objects, loose_bytes) =
        loose_transfer_stats(local_odb, loose_candidates.unwrap_or(installed));
    bytes = bytes.saturating_add(loose_bytes);
    let installed_count = u64::try_from(installed.len()).unwrap_or(u64::MAX);
    let objects = if objects_from_new_packs > 0 {
        objects_from_new_packs.saturating_add(loose_objects)
    } else {
        installed_count
    };
    let kinds = if objects == installed_count && installed_count <= LOCAL_LINK_KINDS_OID_PROBE_MAX {
        Some(object_kind_counts_for_oids(local_odb, installed)?)
    } else {
        None
    };
    Ok(TransferStats {
        objects,
        bytes,
        kinds,
    })
}

/// A single ref update requested by a push.
#[derive(Clone, Debug, Default)]
pub struct PushRefSpec {
    /// The source object to push (`None` for a deletion).
    pub src: Option<ObjectId>,
    /// Local source ref name when known (for reports and `git push --porcelain`).
    pub src_ref: Option<String>,
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
    /// Local ref as the `pre-push` hook names it, when that differs from
    /// [`Self::src_ref`]: `HEAD` for a `HEAD:<dst>` push, where `src_ref` holds the
    /// branch `HEAD` points at. `None` uses `src_ref`.
    pub hook_src_ref: Option<String>,
}

/// Local ref name carried on a push spec for status reporting.
#[must_use]
pub(crate) fn push_spec_local_ref(spec: &PushRefSpec) -> Option<String> {
    spec.src_ref.clone()
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
    if crate::refs::resolve_ref(git_dir, &tag).is_ok() {
        return tag;
    }
    if remote_ref_names.contains(&tag) {
        return tag;
    }
    branch
}

/// Options controlling a push.
#[derive(Clone, Default)]
pub struct PushOptions {
    /// Apply all updates atomically (all-or-nothing), including remote hook rejections.
    ///
    /// Wire pushes require the server's `atomic` capability. If it is absent,
    /// returns [`crate::error::Error::PushAtomicUnsupported`] before sending any
    /// ref-update commands or objects, including with [`Self::dry_run`]. There
    /// is no fallback to a non-atomic push.
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
    /// [`crate::error::Error::PushOptionsUnsupported`] (matching Git). A local-path or
    /// `file://` push ([`push_local`]) accepts them only when the destination repository
    /// sets `receive.advertisePushOptions`, and passes them to its `pre-receive` and
    /// `post-receive` hooks. Either check runs before anything is written, dry runs included.
    pub push_options: Vec<String>,
    /// Diagnostic sink for [`crate::diagnostics::Trace::Network`] events.
    pub diagnostics: Option<crate::diagnostics::DiagnosticsHandle>,
    /// When true with [`Self::diagnostics`], push paths may emit network trace events.
    pub network_trace: bool,
    /// Ref tips already read from the remote, for example while planning
    /// ([`PushPlanContext::remote_tips`](crate::remote::push_plan::PushPlanContext)).
    ///
    /// A local push uses them as the objects the remote already has instead of listing the
    /// remote's refs again; ref updates still check each destination's current value. They
    /// must come from the push destination, or objects the destination lacks may be left out
    /// of the pack. Wire pushes read the advertisement from their connection and ignore this.
    pub remote_ref_tips: Option<Vec<ObjectId>>,
    /// Packs larger than this many bytes go to the server's resumable
    /// `pushBundle` endpoint, when it advertises one, instead of the
    /// receive-pack request. `None` uses `push.bundleThreshold` from config,
    /// else 16 MiB.
    pub resumable_threshold: Option<u64>,
    /// Skip the client-side `pre-push` hook.
    ///
    /// [`crate::remote::Remote::push`] runs the hook (see
    /// [`crate::hooks::run_pre_push_hook`]) after reading the remote's refs and before sending
    /// anything, dry runs included, and fails with [`Error::Hook`] when it exits non-zero. The
    /// lower-level [`push_local`] and [`crate::push::push_remote`] have no repository or
    /// remote name to give the hook and never run it.
    pub no_verify: bool,
}

/// The client-side `pre-push` hook for one push, run once every update has been decided.
pub(crate) struct PrePushGate<'a> {
    /// Repository whose hooks run.
    pub(crate) repo: &'a crate::repo::Repository,
    /// First hook argument: remote name, or the URL for an unnamed remote.
    pub(crate) remote: &'a str,
    /// Second hook argument: the push URL.
    pub(crate) url: &'a str,
}

impl PrePushGate<'_> {
    /// Run the hook over the updates the push would make; refs that are up to date or
    /// already rejected locally are left out, as Git does.
    ///
    /// # Errors
    ///
    /// [`Error::Hook`] when the hook fails or cannot run.
    pub(crate) fn check<'r>(
        &self,
        updates: impl IntoIterator<Item = (&'r PushRefSpec, &'r PushRefResult)>,
    ) -> Result<()> {
        let refs: Vec<crate::hooks::PrePushRef> = updates
            .into_iter()
            .filter(|(_, r)| {
                !matches!(
                    r.status,
                    PushRefStatus::UpToDate
                        | PushRefStatus::RejectNonFastForward
                        | PushRefStatus::RejectStale
                        | PushRefStatus::RejectRemoteRefMissing
                )
            })
            .map(|(spec, r)| crate::hooks::PrePushRef {
                local_ref: if r.deletion {
                    None
                } else {
                    spec.hook_src_ref.clone().or_else(|| r.local_ref.clone())
                },
                local_oid: r.new_oid,
                remote_ref: r.remote_ref.clone(),
                remote_oid: r.old_oid,
            })
            .collect();
        crate::hooks::run_pre_push_hook(self.repo, self.remote, self.url, &refs)?;
        Ok(())
    }
}

/// The structured result of a push. Reuses [`PushRefResult`] for per-ref status.
#[derive(Clone, Debug, Default)]
#[cfg_attr(feature = "serde-models", derive(serde::Serialize))]
pub struct PushOutcome {
    /// Per-ref resolved results (status, old/new oid, reason).
    pub results: Vec<PushRefResult>,
    /// Hook and diagnostic lines from the remote (side-band channel 2), in order.
    ///
    /// For a local-path or `file://` push, the output of the destination's receive hooks.
    /// Empty when the server did not use side-band or no hook wrote anything.
    pub remote_messages: Vec<String>,
}

pub use crate::pack_objects::{
    build_pack, build_pack_from_send_list, reachable_closure, PackBuildOptions, PackObjects,
    PackObjectsOptions, PackStats,
};

pub(crate) use crate::pack_objects::write_pack_for_local_fetch;

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
        mark_promisor_pack: fetch_opts.mark_received_pack_promisor,
        keep_message: None,
        trusted_index_rows: None,
    })
}

/// Incremental [`ProgressPhase::CopyingObjects`] reporting for local fetch/push copies.
struct LocalCopyProgress {
    total: u64,
    done: u64,
    /// Objects examined while deciding which packs qualify (before any are linked).
    pipeline: u64,
    last_reported: u64,
    phase_active: bool,
}

/// Forwards loose-object link [`ProgressSink::inc`] into [`LocalCopyProgress::add`].
struct CopyingLooseLink<'a> {
    copy: &'a mut LocalCopyProgress,
    progress: &'a mut dyn Progress,
}

impl ProgressSink for CopyingLooseLink<'_> {
    fn inc(&mut self, n: u64) {
        self.copy.add(self.progress, n);
    }
}

impl LocalCopyProgress {
    fn new(total: u64) -> Self {
        Self {
            total,
            done: 0,
            pipeline: 0,
            last_reported: 0,
            phase_active: false,
        }
    }

    fn ensure_started(&mut self, progress: &mut dyn Progress) {
        if !self.phase_active {
            progress.start(ProgressPhase::CopyingObjects, Some(self.total));
            self.phase_active = true;
        }
    }

    fn report_scan(&mut self, progress: &mut dyn Progress, n: u64) {
        if n == 0 {
            return;
        }
        self.pipeline = self.pipeline.saturating_add(n).min(self.total);
        let through = self.pipeline.max(self.done);
        self.report_through(progress, through);
        if progress.wants_received_objects() {
            progress.received_objects(ReceivedObjects {
                received: through,
                total: self.total,
                deltas: 0,
            });
        }
    }

    fn report_through(&mut self, progress: &mut dyn Progress, through: u64) {
        let through = through.min(self.total);
        if through <= self.last_reported {
            return;
        }
        self.ensure_started(progress);
        crate::progress::report_object_progress_stride(
            progress,
            through,
            &mut self.last_reported,
            Some(self.total),
        );
    }

    fn add(&mut self, progress: &mut dyn Progress, n: u64) {
        if n == 0 {
            return;
        }
        self.ensure_started(progress);
        self.done = self.done.saturating_add(n).min(self.total);
        let through = self.done.max(self.pipeline);
        self.report_through(progress, through);
        if progress.wants_received_objects() {
            progress.received_objects(ReceivedObjects {
                received: through,
                total: self.total,
                deltas: 0,
            });
        }
    }

    fn finish(&mut self, progress: &mut dyn Progress) {
        if !self.phase_active {
            return;
        }
        let final_count = self.done.max(self.pipeline).min(self.total);
        if self.last_reported < final_count {
            progress.set(final_count);
            self.last_reported = final_count;
        }
        progress.finish();
        self.phase_active = false;
    }
}

/// Copy objects for a local fetch: link remote packfiles that hold only objects
/// the fetch must send, then link or pack any remainder.
///
/// [`FetchOptions::link_source_objects`] instead links the whole source store up
/// front, unreachable objects included (a plain-path clone).
#[allow(clippy::too_many_arguments)]
fn copy_objects_for_local_fetch(
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
    progress: &mut dyn Progress,
    cancel: &dyn crate::progress::Cancel,
    prep_enumerating: bool,
) -> Result<(HashSet<ObjectId>, TransferStats)> {
    use crate::local_object_copy::{
        count_store_objects_for_link, link_loose_objects, link_loose_objects_for_oids,
        link_packs_within, link_remote_pack_files, linked_object_set, refresh_local_odb_after_link,
    };

    let remote_objects = remote_odb.objects_dir();
    let local_objects = local_odb.objects_dir();
    let pack_dir_before = pack_file_basenames(&local_objects.join("pack"));
    let remote_has_alternates = !crate::pack::read_alternates_recursive(remote_objects)?.is_empty();

    if local_odb.uses_files_primary()
        && fetch_opts.link_source_objects
        && !remote_has_alternates
        && !pack_build.thin
        && try_pack_link
        && haves.is_empty()
        && !wants.is_empty()
    {
        crate::progress::poll_cancel(progress, cancel)?;
        let copy_total = count_store_objects_for_link(remote_objects)? as u64;
        let mut copy = LocalCopyProgress::new(copy_total);
        let linked_packs = link_remote_pack_files(
            remote_objects,
            local_objects,
            &mut crate::progress::NullProgress,
            cancel,
            0,
        )?;
        if linked_packs.objects > 0 {
            refresh_local_odb_after_link(local_odb);
            copy.add(progress, linked_packs.objects as u64);
        }
        let pack_alone_satisfies_wants =
            linked_packs.objects > 0 && wants.iter().all(|oid| local_odb.exists(oid));
        let linked_loose = link_loose_objects(
            remote_objects,
            local_objects,
            &mut crate::progress::NullProgress,
            cancel,
        )?;
        if linked_loose > 0 {
            refresh_local_odb_after_link(local_odb);
            copy.add(progress, linked_loose as u64);
        }
        if (linked_packs.files > 0 || linked_loose > 0)
            && wants.iter().all(|oid| local_odb.exists(oid))
        {
            copy.finish(progress);
            let stats_loose = if pack_alone_satisfies_wants {
                HashSet::new()
            } else {
                crate::local_object_copy::loose_reachable_from_roots(
                    local_odb,
                    wants,
                    source_shallow,
                    cancel,
                )?
            };
            let oids = if stats_loose.is_empty() {
                linked_object_set(local_odb, wants)
            } else {
                stats_loose.clone()
            };
            let mut transfer = transfer_stats_for_local_operation(
                local_odb,
                &stats_loose,
                None,
                &pack_dir_before,
            )?;
            enrich_local_link_transfer_kinds(
                local_odb,
                &pack_dir_before,
                &stats_loose,
                &mut transfer,
            )?;
            return Ok((oids, transfer));
        }
        copy.finish(progress);
    }

    let stop = crate::progress::CancelSignal::new(cancel);
    let have_tips: HashSet<ObjectId> = haves.iter().copied().collect();
    if !prep_enumerating {
        progress.start(ProgressPhase::EnumeratingObjects, None);
    }
    let mut enum_last = 0u64;
    let send_set = collect_reachable_excluding_peer(
        remote_odb,
        wants,
        local_odb,
        &have_tips,
        false,
        source_shallow,
        &stop,
        progress,
        &mut enum_last,
    )?;
    let send = &send_set.objects;
    progress.set(send.len() as u64);
    progress.finish();

    let mut copy = LocalCopyProgress::new(send.len() as u64);

    if local_odb.uses_files_primary() && !pack_build.thin && try_pack_link {
        let local_has = |oid: &ObjectId| local_odb.exists_materialized_local_without_rescan(oid);
        let mut on_pack_scan = |n: u64| copy.report_scan(progress, n);
        let linked = link_packs_within(
            remote_objects,
            local_objects,
            send,
            &local_has,
            &mut crate::progress::NullProgress,
            cancel,
            Some(&mut on_pack_scan),
        )?;
        if linked > 0 {
            refresh_local_odb_after_link(local_odb);
            copy.add(progress, linked as u64);
        }
    }

    let mut still_missing = objects_missing_from_peer(local_odb, send);
    if still_missing.is_empty() {
        copy.finish(progress);
        // Every object in `send` is stored locally now, and only packs were installed.
        let oids: HashSet<ObjectId> = send.iter().copied().collect();
        let transfer = transfer_stats_for_local_operation(
            local_odb,
            &oids,
            Some(&HashSet::new()),
            &pack_dir_before,
        )?;
        return Ok((oids, transfer));
    }

    // Hard-link loose objects before building an in-memory pack. Re-reading and
    // recompressing large loose blobs (typical local clone) would spike RSS to several
    // times the object size; linking matches system `git clone` on the same filesystem.
    let loose_candidates: HashSet<ObjectId> = still_missing.iter().copied().collect();
    if local_odb.uses_files_primary() {
        let mut loose_link = CopyingLooseLink {
            copy: &mut copy,
            progress,
        };
        let linked_loose = link_loose_objects_for_oids(
            remote_objects,
            local_objects,
            &still_missing,
            &mut loose_link,
            cancel,
        )?;
        if linked_loose > 0 {
            refresh_local_odb_after_link(local_odb);
            copy.add(progress, linked_loose as u64);
            still_missing = local_odb.missing_oids(still_missing)?;
        }
    }
    if still_missing.is_empty() {
        copy.finish(progress);
        // Every object in `send` is stored locally now; the loose links covered the rest.
        let oids: HashSet<ObjectId> = send.iter().copied().collect();
        let transfer = transfer_stats_for_local_operation(
            local_odb,
            &oids,
            Some(&loose_candidates),
            &pack_dir_before,
        )?;
        return Ok((oids, transfer));
    }

    let pack_opts = pack_build_options_for_send(remote_odb, &still_missing, pack_build);
    crate::progress::poll_cancel(progress, cancel)?;
    // Stream the pack into a spool in `objects/pack` rather than memory, as a local push
    // does; a cancelled or failed copy removes the spool.
    let mut spool = crate::pack_receive::TempPackReceive::create_in_pack_dir(local_objects)?;
    write_pack_for_local_transfer(
        remote_odb,
        &send_set,
        &still_missing,
        local_odb,
        haves,
        source_shallow,
        have_shallow,
        &pack_opts,
        &stop,
        &mut spool,
        Some((&mut copy, progress)),
    )?;
    copy.finish(progress);
    crate::progress::poll_cancel(progress, cancel)?;
    let ingested = match spool.finish()? {
        Some(pack_path) => {
            let ingest_opts = ingest_pack_options_for_fetch(local_git_dir, fetch_opts)?;
            crate::index_pack::ingest_received_pack_path_with_progress(
                pack_path,
                local_odb,
                &ingest_opts,
                Some(progress),
                Some(cancel),
            )?
        }
        None => crate::index_pack::IngestedPack::default(),
    };
    refresh_local_odb_after_link(local_odb);
    // Every object in `send` is stored locally now: linked, or in the new pack.
    let mut out: HashSet<ObjectId> = send.iter().copied().collect();
    out.extend(ingested.object_ids);
    let transfer = transfer_stats_for_local_operation(local_odb, &out, None, &pack_dir_before)?;
    Ok((out, transfer))
}

/// Copy objects for a local push: link source packfiles that hold only objects
/// the push must send into `dest_odb`, then pack any remainder.
///
/// `peer_odb` is the remote's own object store: the walk stops at objects it already
/// has, and it answers for `haves`. `dest_odb` receives the copied objects (the
/// remote's receive quarantine) and must read `peer_odb` as an alternate.
///
/// The remainder is streamed into a `tmp_pack_*` spool in `dest_odb`'s `pack/`
/// directory and indexed from there, so the pack is never held in memory. `cancel`
/// is polled throughout the walk, the linking, the pack build and the indexing; a
/// cancelled copy removes its spool and staging files and publishes no pack.
#[allow(clippy::too_many_arguments)]
fn copy_objects_for_local_push(
    local_odb: &Odb,
    peer_odb: &Odb,
    dest_odb: &Odb,
    wants: &[ObjectId],
    haves: &[ObjectId],
    source_shallow: &HashSet<ObjectId>,
    have_shallow: &HashSet<ObjectId>,
    opts: &PackBuildOptions,
    cancel: &dyn crate::progress::Cancel,
) -> Result<()> {
    use crate::local_object_copy::{
        link_loose_objects_for_oids, link_packs_within, refresh_local_odb_after_link,
    };

    let local_objects = local_odb.objects_dir();
    let dest_objects = dest_odb.objects_dir();
    let stop = crate::progress::CancelSignal::new(cancel);

    let have_tips: HashSet<ObjectId> = haves.iter().copied().collect();
    let mut enum_last = 0u64;
    let mut enum_sink = crate::progress::NullProgress;
    let send_set = collect_reachable_excluding_peer(
        local_odb,
        wants,
        peer_odb,
        &have_tips,
        false,
        source_shallow,
        &stop,
        &mut enum_sink,
        &mut enum_last,
    )?;
    let send = &send_set.objects;

    let mut progress = crate::progress::NullProgress;
    let dest_has = |oid: &ObjectId| dest_odb.exists_materialized_local_without_rescan(oid);
    if local_odb.uses_files_primary()
        && !opts.thin
        && link_packs_within(
            local_objects,
            dest_objects,
            send,
            &dest_has,
            &mut progress,
            cancel,
            None,
        )? > 0
    {
        refresh_local_odb_after_link(dest_odb);
    }

    let mut still_missing = objects_missing_from_peer(dest_odb, send);
    if still_missing.is_empty() {
        return Ok(());
    }

    if local_odb.uses_files_primary() && !haves.is_empty() {
        let linked_loose = link_loose_objects_for_oids(
            local_objects,
            dest_objects,
            &still_missing,
            &mut progress,
            cancel,
        )?;
        if linked_loose > 0 {
            refresh_local_odb_after_link(dest_odb);
            still_missing = dest_odb.missing_oids(still_missing)?;
        }
        if still_missing.is_empty() {
            return Ok(());
        }
    }

    let pack_opts = pack_build_options_for_send(local_odb, &still_missing, opts);
    let mut spool = crate::pack_receive::TempPackReceive::create_in_pack_dir(dest_objects)?;
    write_pack_for_local_transfer(
        local_odb,
        &send_set,
        &still_missing,
        peer_odb,
        haves,
        source_shallow,
        have_shallow,
        &pack_opts,
        &stop,
        &mut spool,
        None,
    )?;
    let Some(pack_path) = spool.finish()? else {
        return Ok(());
    };
    // Indexing hashes every object and checks the trailer, as receive-pack does, so
    // the second whole-pack verification pass is skipped.
    crate::index_pack::ingest_received_pack_path_with_progress(
        pack_path,
        dest_odb,
        &crate::index_pack::IngestPackOptions {
            fix_thin: pack_opts.thin,
            skip_post_index_verify: true,
            ..Default::default()
        },
        None,
        Some(cancel),
    )?;
    refresh_local_odb_after_link(dest_odb);
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

/// Stream the pack that copies `missing` (a subset of `send_set`) into the peer to `w`.
///
/// For a non-thin pack, `missing` is already the exact set the peer lacks (the walk
/// stopped at every object the peer stores, with or without `haves`), so it is packed
/// as-is. Re-enumerating from `missing` instead would walk its whole closure again (for
/// a fetch with `haves`, a second reachability pass over everything the peer has) and
/// re-send objects just linked into the peer, such as every packed object reachable
/// from a loose annotated tag. A thin pack, or a shallow peer, defers to
/// [`write_pack_for_local_fetch`], which negotiates against the peer's `haves`.
/// Stops with [`Error::Cancelled`] once `stop` fires.
#[allow(clippy::too_many_arguments)]
fn write_pack_for_local_transfer(
    source_odb: &Odb,
    send_set: &PeerSendSet,
    missing: &[ObjectId],
    peer_odb: &Odb,
    haves: &[ObjectId],
    source_shallow: &HashSet<ObjectId>,
    have_shallow: &HashSet<ObjectId>,
    opts: &PackBuildOptions,
    stop: &crate::progress::CancelSignal<'_>,
    w: &mut dyn Write,
    copy: Option<(&mut LocalCopyProgress, &mut dyn Progress)>,
) -> Result<()> {
    if !opts.thin && have_shallow.is_empty() {
        let pack_budget = missing.len() as u64;
        if let Some((copy, progress)) = copy {
            let mut pack_reported = 0u64;
            let mut on_pack = |n: u64| {
                let add = n.min(pack_budget.saturating_sub(pack_reported));
                if add > 0 {
                    pack_reported += add;
                    copy.add(progress, add);
                }
            };
            crate::pack_objects::write_pack_from_send_list(
                source_odb,
                missing,
                opts,
                &send_set.name_hashes,
                stop,
                w,
                Some(&mut on_pack),
            )?;
        } else {
            crate::pack_objects::write_pack_from_send_list(
                source_odb,
                missing,
                opts,
                &send_set.name_hashes,
                stop,
                w,
                None,
            )?;
        }
    } else {
        write_pack_for_local_fetch(
            source_odb,
            missing,
            peer_odb,
            haves,
            source_shallow,
            have_shallow,
            opts,
            stop,
            w,
        )?;
    }
    Ok(())
}

/// Objects a local transfer must send, from [`collect_reachable_excluding_peer`].
struct PeerSendSet {
    /// Objects missing from the peer, in walk order.
    objects: Vec<ObjectId>,
    /// Pack name hash of each blob's and subtree's path, for delta-window ordering.
    name_hashes: HashMap<ObjectId, u32>,
}

/// Like [`collect_reachable_excluding`], but treats every object already stored in
/// `peer_odb` as a boundary (plus explicit `peer_boundary` tips). Used for local
/// file fetch/push so incremental transfers do not walk full history from `haves`.
/// `stop` is polled for every object read.
#[allow(clippy::too_many_arguments)]
fn collect_reachable_excluding_peer(
    source_odb: &Odb,
    roots: &[ObjectId],
    peer_odb: &Odb,
    peer_boundary: &HashSet<ObjectId>,
    skip_missing: bool,
    shallow_grafts: &HashSet<ObjectId>,
    stop: &crate::progress::CancelSignal<'_>,
    enum_sink: &mut dyn ProgressSink,
    enum_last: &mut u64,
) -> Result<PeerSendSet> {
    use crate::pack_name_hash::pack_name_hash_extend;

    stop.poll()?;
    if peer_boundary.is_empty() && shallow_grafts.is_empty() {
        if let Some(send_set) = bitmap_send_set(source_odb, roots, peer_odb, enum_sink, enum_last)?
        {
            stop.poll()?;
            return Ok(send_set);
        }
    }

    let mut visited: HashSet<ObjectId> = HashSet::new();
    let mut ordered: Vec<ObjectId> = Vec::new();
    let mut name_hashes: HashMap<ObjectId, u32> = HashMap::new();
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
        stop.poll()?;
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
                    let queued = if entry.mode & 0o170000 == 0o040000 {
                        let dir_hash = pack_name_hash_extend(path_hash, b"/");
                        enqueue(
                            entry.oid,
                            Some(dir_hash),
                            &mut queue,
                            &mut visited,
                            &mut ordered,
                        )
                    } else {
                        enqueue(entry.oid, None, &mut queue, &mut visited, &mut ordered)
                    };
                    if queued {
                        name_hashes.insert(entry.oid, path_hash);
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
        let n = ordered.len() as u64;
        crate::progress::report_object_progress_stride(enum_sink, n, enum_last, None);
    }

    Ok(PeerSendSet {
        objects: ordered,
        name_hashes,
    })
}

/// The send set of a transfer with no peer tips, answered from the source's
/// reachability bitmap instead of reading every commit and tree.
///
/// Returns `None` when the source has no usable bitmap.
fn bitmap_send_set(
    source_odb: &Odb,
    roots: &[ObjectId],
    peer_odb: &Odb,
    enum_sink: &mut dyn ProgressSink,
    enum_last: &mut u64,
) -> Result<Option<PeerSendSet>> {
    let no_haves = HashSet::new();
    let Some((reachable, _)) = crate::pack_object_select::try_bitmap_reachable_for_upload(
        source_odb,
        roots,
        &[],
        None,
        &no_haves,
    )?
    else {
        return Ok(None);
    };
    peer_odb.reprepare_local_packs();
    let mut objects: Vec<ObjectId> = Vec::new();
    for oid in reachable
        .iter_grouped_by_kind()
        .filter(|oid| !peer_odb.exists_materialized_local_without_rescan(oid))
    {
        objects.push(oid);
        let n = objects.len() as u64;
        crate::progress::report_object_progress_stride(enum_sink, n, enum_last, None);
    }
    let name_hashes = crate::pack_object_select::name_hashes_for_send(
        reachable.bitmap_index(),
        &objects,
        &no_haves,
    );
    Ok(Some(PeerSendSet {
        objects,
        name_hashes,
    }))
}

/// `fetch_local` function.
pub fn fetch_local(
    local_git_dir: &Path,
    remote_git_dir: &Path,
    opts: &FetchOptions,
) -> Result<FetchOutcome> {
    fetch_local_with_progress(
        local_git_dir,
        remote_git_dir,
        opts,
        &mut NoProgress,
        &crate::progress::NeverCancel,
    )
}

/// Result of matching remote refs to fetch refspecs.
pub(crate) struct FetchMatchSet {
    pub(crate) matched: Vec<MatchedRef>,
    pub(crate) positive: Vec<RefspecItem>,
    pub(crate) negatives: Vec<RefspecItem>,
    pub(crate) explicit_refspec_fetch: bool,
}

/// Match remote refs to fetch refspecs (shared by local, wire, and bundle fetch).
pub(crate) fn build_fetch_matched_set(
    opts: &FetchOptions,
    remote_refs: &[(String, ObjectId)],
    remote_head_oid: Option<ObjectId>,
) -> Result<FetchMatchSet> {
    use crate::fetch_request::{FetchRefspecInput, FetchRefspecPlan, FetchRemoteAdvertisement};

    let remote = FetchRemoteAdvertisement {
        refs: remote_refs,
        head_oid: remote_head_oid,
    };
    let plan = FetchRefspecPlan::resolve(
        &FetchRefspecInput {
            user_refspecs: opts.refspecs.clone(),
            configured_refspecs: opts.configured_refspecs.clone(),
            remote_name: opts.remote_name.clone(),
            allow_missing_explicit_for_prune: opts.prune,
        },
        remote,
    )?;
    let explicit_refspec_fetch = plan.is_explicit();

    let mut negatives = match &plan {
        FetchRefspecPlan::Pattern { negatives, .. } | FetchRefspecPlan::Mixed { negatives, .. } => {
            negatives.clone()
        }
    };
    for spec in &opts.negative_refspecs {
        let item = parse_fetch_refspec(spec)
            .map_err(|e| Error::Message(format!("invalid negative refspec '{spec}': {e}")))?;
        negatives.push(item);
    }

    let mut positive = match &plan {
        FetchRefspecPlan::Pattern { positive, .. } => positive.clone(),
        FetchRefspecPlan::Mixed {
            pattern_positive, ..
        } => {
            let mut positive = pattern_positive.clone();
            for spec in &opts.refspecs {
                if let Ok(item) = parse_fetch_refspec(spec) {
                    if !item.negative && !item.pattern && !item.matching {
                        positive.push(item);
                    }
                }
            }
            positive
        }
    };
    if opts.prune {
        crate::fetch_request::supplement_positive_for_explicit_prune(
            &mut positive,
            &opts.configured_refspecs,
            opts.remote_name.as_deref(),
        );
    }

    let matched =
        crate::fetch_request::matched_refs_with_negatives(&plan, remote, &negatives, opts.prune)?;
    Ok(FetchMatchSet {
        matched,
        positive,
        negatives,
        explicit_refspec_fetch,
    })
}

/// Enforce Git fetch ref-update rules for explicit refspec fetches.
pub(crate) fn enforce_fetch_ref_update(
    local_ref: &str,
    old: Option<ObjectId>,
    new: ObjectId,
    mode: UpdateMode,
    opts: &FetchOptions,
    explicit_refspec_fetch: bool,
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
    let _ = explicit_refspec_fetch;
    Ok(())
}

/// Fail with [`Error::RemoteUnreadable`] unless `remote_git_dir` is a Git directory.
///
/// A missing or non-repository path must not read as an empty remote.
pub(crate) fn ensure_local_remote_repository(remote_git_dir: &Path) -> Result<()> {
    if crate::submodule_gitdir::is_valid_git_dir(remote_git_dir) {
        return Ok(());
    }
    Err(Error::remote_unreadable(
        remote_git_dir.display().to_string(),
        RemoteUnreadableReason::RepositoryNotFound,
    ))
}

/// Like [`fetch_local`], reporting object copy progress through `progress`.
///
/// The local repository is opened with [`FetchOptions::environment`], so objects land in its
/// `GIT_OBJECT_DIRECTORY` when that is set.
pub fn fetch_local_with_progress(
    local_git_dir: &Path,
    remote_git_dir: &Path,
    opts: &FetchOptions,
    progress: &mut dyn Progress,
    cancel: &dyn crate::progress::Cancel,
) -> Result<FetchOutcome> {
    crate::progress::poll_cancel(progress, cancel)?;
    ensure_local_remote_repository(remote_git_dir)?;
    let local = crate::fetch::open_fetch_target(local_git_dir, opts)?;
    fetch_local_into(&local, remote_git_dir, opts, progress, cancel)
}

/// Like [`fetch_local_with_progress`], copying objects into `local`'s object store.
pub(crate) fn fetch_local_into(
    local: &crate::repo::Repository,
    remote_git_dir: &Path,
    opts: &FetchOptions,
    progress: &mut dyn Progress,
    cancel: &dyn crate::progress::Cancel,
) -> Result<FetchOutcome> {
    crate::progress::poll_cancel(progress, cancel)?;
    ensure_local_remote_repository(remote_git_dir)?;
    let local_git_dir = local.git_dir();
    validate_fetch_depth_local(local_git_dir, opts)?;

    let remote_odb = open_odb(remote_git_dir);
    let remote_algo = remote_odb.hash_algo();
    let reopened = sync_empty_destination_object_format(local_git_dir, remote_algo, opts)?
        .then(|| reopen_after_format_change(local))
        .transpose()?;
    let local = reopened.as_ref().unwrap_or(local);
    let local_git_dir = local.git_dir();
    let local_odb = local.odb();

    // 1. Enumerate remote refs (with HEAD symref for the default branch).
    let remote_entries = crate::remote::list_refs_from_git_dir_for_fetch(
        remote_git_dir,
        Some(&remote_odb),
        &opts.refspecs,
    )
    .map_err(crate::error::Error::from)?;

    let mut default_branch = None;
    let mut remote_head_oid = None;
    // remote ref name -> oid (excluding HEAD and peeled `^{}` entries).
    let mut remote_refs: Vec<(String, ObjectId)> = Vec::new();
    for entry in &remote_entries {
        if entry.name == "HEAD" {
            remote_head_oid = Some(entry.oid);
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

    let FetchMatchSet {
        mut matched,
        positive,
        negatives,
        explicit_refspec_fetch,
    } = build_fetch_matched_set(opts, &remote_refs, remote_head_oid)?;
    let mut matched_oids: HashSet<ObjectId> = matched.iter().map(|m| m.oid).collect();
    let mut seen_remote_ref: HashSet<String> =
        matched.iter().map(|m| m.remote_ref.clone()).collect();

    let local_shallow = crate::shallow::load_shallow_boundaries(local_git_dir);
    let remote_shallow = crate::shallow::load_shallow_boundaries(remote_git_dir);

    let mut prep_enumerating = false;
    if !opts.dry_run {
        progress.start(ProgressPhase::EnumeratingObjects, None);
        prep_enumerating = true;
    }

    // Explicitly requested tags participate in depth calculation; automatically
    // followed tags must only use the history selected by those fetch roots.
    if opts.tags == TagMode::All {
        apply_tag_mode(
            opts.tags,
            &remote_refs,
            &remote_odb,
            &remote_shallow,
            &negatives,
            &HashSet::new(),
            &mut matched,
            &mut matched_oids,
            &mut seen_remote_ref,
        )?;
    }
    let shallow_response = if opts.has_deepen_request() {
        let shallow_wants: HashSet<ObjectId> = matched
            .iter()
            .map(|m| shallow_want_commit_tip(&remote_odb, m))
            .collect::<Result<_>>()?;
        let remote_repo =
            crate::repo::Repository::open(remote_git_dir, &RepositoryOptions::empty())?;
        let (wire_depth, deepen_relative) = opts
            .wire_deepen()
            .map(|(d, r)| (Some(d), r))
            .unwrap_or((None, false));
        let req = crate::serve::ShallowRequest {
            client_shallow: local_shallow.iter().copied().collect(),
            depth: wire_depth,
            deepen_since: opts
                .deepen_since
                .as_deref()
                .and_then(|s| s.parse::<i64>().ok()),
            deepen_not: Vec::new(),
            deepen_relative,
        };
        Some(crate::serve::compute_shallow_response(
            &remote_repo,
            &shallow_wants.into_iter().collect::<Vec<_>>(),
            &req,
        )?)
    } else {
        None
    };
    let copy_source_shallow = shallow_response
        .as_ref()
        .map(|resp| resp.pack_shallow_grafts.clone())
        .unwrap_or_else(|| remote_shallow.clone());
    let mut tag_shallow = copy_source_shallow.clone();
    if shallow_response.is_none() {
        tag_shallow.extend(local_shallow.iter().copied());
    }

    let following_only = if opts.tags == TagMode::Following {
        let tags_present = crate::fetch::tags_already_local(local.refs(), &remote_refs);
        apply_tag_mode(
            opts.tags,
            &remote_refs,
            &remote_odb,
            &tag_shallow,
            &negatives,
            &tags_present,
            &mut matched,
            &mut matched_oids,
            &mut seen_remote_ref,
        )?
    } else {
        HashSet::new()
    };

    let mut following_tag_wants = if shallow_response.is_some() {
        // Branch roots already bring in lightweight targets. Only annotated tag
        // objects (and their chains) need additional wants in this response.
        local_odb
            .missing_oids(
                matched
                    .iter()
                    .filter(|m| {
                        following_only.contains(&m.oid)
                            && m.advertised_peel.is_some_and(|peel| peel != m.oid)
                    })
                    .map(|m| m.oid),
            )?
            .into_iter()
            .collect()
    } else {
        crate::fetch::following_tag_wants_with_local_peel(&matched, &following_only, local_odb)?
    };
    let shallow_request = opts.has_deepen_request() || !local_shallow.is_empty();
    if shallow_request && shallow_response.is_none() {
        let fetch_commit_oids = matched
            .iter()
            .filter(|m| !m.is_tag)
            .map(|m| m.oid)
            .collect();
        let peel_set = crate::fetch::following_peel_commit_set_for_shallow(
            &remote_odb,
            &fetch_commit_oids,
            opts.absolute_depth_for_tags(),
            &tag_shallow,
        )?;
        following_tag_wants.extend(
            crate::fetch::following_tag_wants_peel_among_fetched_commits(
                &matched,
                &following_only,
                &peel_set,
            ),
        );
    }

    // Determine wants and copy only the required object set.
    let mut wants: Vec<ObjectId> = if shallow_request {
        matched_oids
            .iter()
            .copied()
            .filter(|oid| !following_only.contains(oid) || following_tag_wants.contains(oid))
            .collect()
    } else {
        local_odb.missing_oids(matched_oids.iter().copied())?
    };
    let (shallow_new, shallow_unshallow) = if let Some(resp) = shallow_response {
        for oid in resp.extra_wants.iter().chain(resp.shallow.iter()).copied() {
            if !wants.contains(&oid) {
                wants.push(oid);
            }
        }
        (resp.shallow, resp.unshallow)
    } else {
        (Vec::new(), Vec::new())
    };
    let mut pack_oids: HashSet<ObjectId> = HashSet::new();
    let mut transfer = TransferStats::default();
    if wants.is_empty() && prep_enumerating {
        progress.finish();
        prep_enumerating = false;
    }
    if !wants.is_empty() && !opts.dry_run {
        // Resolve exclusion tips only once the final shallow/deepen wants need a copy.
        let mut haves: Vec<ObjectId> = Vec::new();
        let mut have_seen: HashSet<ObjectId> = HashSet::new();
        for m in &matched {
            if let Some(local_ref) = &m.local_ref {
                if let Ok(old) = local.resolve_ref_oid(local_ref) {
                    if have_seen.insert(old) {
                        haves.push(old);
                    }
                }
            }
        }

        let remote_cfg = config_for_git_dir(remote_git_dir);
        let pack_opts = PackBuildOptions::for_local_copy(remote_cfg.as_ref());
        let (oids, fetch_transfer) = copy_objects_for_local_fetch(
            local_git_dir,
            &remote_odb,
            local_odb,
            &wants,
            &haves,
            &copy_source_shallow,
            &local_shallow,
            !opts.has_deepen_request(),
            &pack_opts,
            opts,
            progress,
            cancel,
            prep_enumerating,
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
        crate::fetch::retain_following_tags(local_odb, &mut matched, &pack_oids, &boundaries)?;
    }

    // 4. Classify and apply ref updates. Ancestry checks use the local repo
    //    (or the bare remote during `--dry-run`, when objects are not copied).
    let dry_run_remote = if opts.dry_run {
        crate::repo::Repository::open(remote_git_dir, &RepositoryOptions::empty()).ok()
    } else {
        None
    };
    let classify_repo = if opts.dry_run {
        dry_run_remote.as_ref()
    } else {
        Some(local)
    };

    let mut updates: Vec<RefUpdate> = Vec::new();

    // Prune BEFORE writing the new tips. A stale tracking ref stored as a file
    // (e.g. `refs/remotes/origin/a`) otherwise blocks creating a nested ref the
    // same fetch introduces (`refs/remotes/origin/a/b`) with a "File exists"
    // directory/file conflict (matches `git fetch --prune` ordering).
    if opts.prune {
        prune_tracking_refs(local, &positive, &remote_refs, opts.dry_run, &mut updates)?;
    }

    let mut store_batch: Vec<(String, crate::refs::store::RawRef)> =
        Vec::with_capacity(matched.len());
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

        let old = local.resolve_ref_oid(local_ref).ok();
        let mode = classify_update(
            old.as_ref(),
            &m.oid,
            m.force,
            m.dest_is_tag,
            classify_repo,
            None,
        );

        let write = matches!(
            mode,
            UpdateMode::New | UpdateMode::FastForward | UpdateMode::Forced
        );
        enforce_fetch_ref_update(
            local_ref,
            old,
            m.oid,
            mode,
            opts,
            explicit_refspec_fetch,
            classify_repo,
        )?;
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
            if m.dest_is_tag && !ref_target_exists(local_odb, &remote_odb, m.oid)? {
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
            store_batch.push((local_ref.clone(), crate::refs::store::RawRef::Direct(m.oid)));
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
                if crate::refs::resolve_ref_in(local, &head_ref).is_err() {
                    let target = format!("refs/remotes/{remote}/{branch}");
                    let target_will_exist = store_batch.iter().any(|(name, _)| *name == target)
                        || crate::refs::resolve_ref_in(local, &target).is_ok();
                    if target_will_exist {
                        store_batch.push((head_ref, crate::refs::store::RawRef::Symbolic(target)));
                    }
                }
            }
        }
    }
    if !opts.dry_run && !store_batch.is_empty() {
        apply_fetch_ref_updates(
            local,
            store_batch,
            &mut updates,
            FetchRefRejectHint {
                prune: opts.prune,
                positive: &positive,
                remote_refs: &remote_refs,
                remote_name: opts.remote_name.as_deref(),
            },
            opts.initial_remote_fetch,
        )?;
    }

    crate::fetch::finish_initial_remote_fetch_layout(local, opts, default_branch.as_deref())?;
    if let Some(remote) = opts.remote_name.as_deref() {
        crate::fetch::ensure_remote_tracking_head_symref(
            local,
            remote,
            default_branch.as_deref(),
            opts.dry_run,
        )?;
    }
    finish_anonymous_url_fetch(
        local_git_dir,
        opts,
        explicit_refspec_fetch,
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
        filter_sent: false,
        promisor: None,
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
///    LOCAL odb into a quarantine of the REMOTE odb via [`build_pack`] and
///    [`crate::index_pack::ingest_received_pack`], excluding objects already
///    reachable from the remote's existing ref tips.
/// 4. Applies the remote's `receive.*` policy (deny non-fast-forwards, deletes and the
///    current branch, hidden refs) as [`crate::serve::receive_pack`] does.
/// 5. Runs the remote's receive hooks with Git's arguments, stdin and environment:
///    `pre-receive` over every update (with the quarantine variables, so it can read
///    the pushed objects), then `update` per ref once the objects are installed. A
///    non-zero `pre-receive` refuses every update and keeps the objects out of the
///    remote; a non-zero `update` refuses its ref. Refusals are
///    [`PushRefStatus::RemoteRejected`] results, not errors.
/// 6. Applies the ref changes on the remote as a compare-and-swap against the value
///    read in step 1, like receive-pack: an update or delete needs the ref to still
///    hold that oid, and a create needs it to still be absent, even with `force`. A
///    ref that another writer created in between is reported as
///    [`PushRefStatus::RejectAlreadyExists`]; one that moved is
///    [`PushRefStatus::RejectStale`]. Then `post-receive` and `post-update` run over
///    the refs that were written.
///
/// Objects are copied into a quarantine directory first and move into the remote's
/// object store only when some update is going ahead. With `opts.dry_run` nothing is
/// copied or written and no receive hook runs, as with `git push --dry-run`. Hook
/// output is returned in [`PushOutcome::remote_messages`]. Through
/// [`crate::remote::Remote::push`], hooks run with the pushing repository's environment
/// minus the variables that locate it; `push_local` opens the local repository with
/// [`RepositoryOptions::empty`], so the hooks it runs get an empty environment (no `PATH`).
///
/// When `opts.atomic` is set and any ref is rejected (by the update rules, the
/// remote's policy or a hook), no ref is written and every otherwise-accepted ref is
/// reported as [`PushRefStatus::AtomicPushFailed`].
///
/// Both repositories must use the same object hash algorithm; the hash width is
/// threaded through [`Odb::hash_algo`] so SHA-256 repos work.
///
/// The client-side `pre-push` hook is not run here; [`crate::remote::Remote::push`] runs it.
///
/// # Errors
///
/// Returns an error if either repository cannot be opened, if a source object is
/// missing from the local odb, or on I/O failure while writing objects or refs.
/// Returns [`Error::PushOptionsUnsupported`] when `opts.push_options` is non-empty and
/// the remote does not set `receive.advertisePushOptions`.
//
// TODO(phase: remote transports): `git://`, `http(s)`, and `ssh` push
// (receive-pack handshake + report-status parsing + credential helpers) are out
// of scope here and live in a later phase.
pub fn push_local(
    local_git_dir: &Path,
    remote_git_dir: &Path,
    refs: &[PushRefSpec],
    opts: &PushOptions,
    cancel: &dyn crate::progress::Cancel,
) -> Result<PushOutcome> {
    let local = crate::repo::Repository::open(local_git_dir, &RepositoryOptions::empty())?;
    push_local_from(
        &local,
        remote_git_dir,
        refs,
        opts,
        &mut crate::fetch::NoProgress,
        cancel,
        None,
    )
}

/// Like [`push_local`], reading the objects to send through `local`'s object store,
/// running `pre_push` after every update is decided and before anything is written to the
/// remote, and relaying the remote's hook output to [`crate::fetch::Progress::message`] as a
/// wire push relays side-band output.
pub(crate) fn push_local_from(
    local: &crate::repo::Repository,
    remote_git_dir: &Path,
    refs: &[PushRefSpec],
    opts: &PushOptions,
    progress: &mut dyn crate::fetch::Progress,
    cancel: &dyn crate::progress::Cancel,
    pre_push: Option<&PrePushGate<'_>>,
) -> Result<PushOutcome> {
    let local_git_dir = local.git_dir();
    let mut progress_sink = crate::progress::NullProgress;
    crate::progress::poll_cancel(&mut progress_sink, cancel)?;
    ensure_local_remote_repository(remote_git_dir)?;
    let remote_repo =
        crate::repo::Repository::open(remote_git_dir, &local.peer_repository_options())?;
    let remote_cfg = remote_repo
        .config()
        .unwrap_or_else(|_| std::sync::Arc::new(crate::config::ConfigSet::new()));
    if !opts.push_options.is_empty()
        && !remote_cfg
            .get_bool("receive.advertisepushoptions")
            .and_then(|r| r.ok())
            .unwrap_or(false)
    {
        return Err(Error::PushOptionsUnsupported);
    }

    let local_odb = local.odb();

    // The remote's existing ref tips become the `haves` for pack building, so the
    // copied object closure excludes everything the remote already has.
    let remote_have_tips: Vec<ObjectId> = match &opts.remote_ref_tips {
        Some(tips) => tips.clone(),
        None => crate::refs::list_refs(remote_git_dir, "refs/")?
            .into_iter()
            .map(|(_, oid)| oid)
            .collect(),
    };

    // First pass: decide each ref's status without mutating anything.
    let mut decisions: Vec<PushDecision> = Vec::with_capacity(refs.len());
    for spec in refs {
        decisions.push(decide_push(spec, local_odb, remote_git_dir, Some(local))?);
    }
    if let Some(gate) = pre_push {
        gate.check(refs.iter().zip(decisions.iter().map(|d| &d.result)))?;
    }

    let push_wants: Vec<ObjectId> = decisions
        .iter()
        .filter(|d| d.apply && !opts.dry_run)
        .filter_map(|d| match &d.action {
            PushAction::Update(src) => Some(*src),
            _ => None,
        })
        .collect();
    // Pushed objects wait in a quarantine until the receive hooks accept the push, so a
    // declined or refused push leaves nothing behind in the remote's object store.
    let mut quarantine = None;
    if !push_wants.is_empty() {
        crate::progress::poll_cancel(&mut progress_sink, cancel)?;
        let local_cfg = config_for_git_dir(local_git_dir);
        let pack_opts = if remote_have_tips.is_empty() {
            PackBuildOptions::for_local_copy(local_cfg.as_ref())
        } else {
            PackBuildOptions::for_local_push(local_cfg.as_ref())
        };
        let empty_shallow = HashSet::new();
        let remote_shallow = crate::shallow::load_shallow_boundaries(remote_git_dir);
        let q = crate::receive_quarantine::ReceiveQuarantine::create(
            remote_repo.odb(),
            remote_git_dir,
        )?;
        let q_odb = q.odb();
        copy_objects_for_local_push(
            local_odb,
            remote_repo.odb(),
            &q_odb,
            &push_wants,
            &remote_have_tips,
            &empty_shallow,
            &remote_shallow,
            &pack_opts,
            cancel,
        )?;
        quarantine = Some((q, q_odb));
    }

    // What a receive-pack would be sent: every update still going ahead, before the
    // remote's own checks refuse any of them.
    let received: Vec<crate::serve::RefCommand> = if opts.dry_run {
        Vec::new()
    } else {
        decisions
            .iter()
            .filter(|d| d.apply)
            .filter_map(PushDecision::ref_command)
            .collect()
    };

    #[cfg(test)]
    tests::run_push_hook(tests::PushHookPoint::BeforeReceivePolicy);

    apply_remote_receive_policy(
        &remote_repo,
        local_git_dir,
        opts.dry_run,
        quarantine.as_ref().map(|(_, odb)| odb),
        &mut decisions,
    )?;

    if opts.atomic && decisions.iter().any(|d| d.result.status.is_error()) {
        fail_atomic_push(&mut decisions);
        return Ok(PushOutcome {
            results: decisions.into_iter().map(|d| d.result).collect(),
            remote_messages: Vec::new(),
        });
    }

    let scripts =
        crate::serve::ReceiveHookScripts::new(&remote_repo, &opts.push_options, Vec::new());
    let mut remote_messages = Vec::new();
    if !received.is_empty() {
        let commands: Vec<&crate::serve::RefCommand> = received.iter().collect();
        let pre = scripts.pre_receive(&commands, quarantine.as_ref().map(|(q, _)| q));
        relay_hook_output(progress, &mut remote_messages, &pre);
        if !pre.is_ok() {
            for d in decisions.iter_mut().filter(|d| d.apply) {
                d.refuse(&crate::serve::RefUpdateRefusal::PreReceiveHookDeclined);
            }
        }
    }
    let mut migrated_pack_keeps = Vec::new();
    if decisions.iter().any(|d| d.apply) && !opts.dry_run {
        if let Some((mut q, _)) = quarantine.take() {
            migrated_pack_keeps = q.migrate()?;
            remote_repo.odb().invalidate_packs();
        }
        for d in decisions.iter_mut().filter(|d| d.apply) {
            let Some(cmd) = d.ref_command() else {
                continue;
            };
            let update = scripts.update(&cmd);
            relay_hook_output(progress, &mut remote_messages, &update);
            if !update.is_ok() {
                d.refuse(&crate::serve::RefUpdateRefusal::UpdateHookDeclined);
                // receive-pack names the declined ref to the pusher.
                // hygiene: Git wire-compatible remote message, relayed like receive-pack's
                let line = format!("error: hook declined to update {}", d.result.remote_ref);
                progress.message(format!("{line}\n").as_bytes());
                remote_messages.push(line);
            }
        }
        if opts.atomic && decisions.iter().any(|d| d.result.status.is_error()) {
            fail_atomic_push(&mut decisions);
        }
    }

    #[cfg(test)]
    tests::run_push_hook(tests::PushHookPoint::BeforeRefCommit);

    // Second pass: apply ref updates (one batch unless `atomic`, then batched non-atomic).
    let mut ref_batch: Vec<crate::refs::store::RefUpdate> = Vec::new();
    let mut non_atomic_batch: Vec<(usize, crate::refs::store::RefUpdate)> = Vec::new();
    for (idx, d) in decisions.iter_mut().enumerate() {
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
                } else {
                    non_atomic_batch.push((idx, update));
                }
            }
            PushAction::Delete => {
                let update =
                    push_store_ref_update(d.result.remote_ref.as_str(), None, d.result.old_oid);
                if opts.atomic {
                    ref_batch.push(update);
                } else {
                    non_atomic_batch.push((idx, update));
                }
            }
            PushAction::None => {}
        }
    }
    if opts.atomic && !opts.dry_run && !ref_batch.is_empty() {
        if let Err(err) = crate::refs::commit_ref_store_batch(remote_git_dir, &ref_batch) {
            apply_atomic_batch_ref_store_error(&mut decisions, &ref_batch, err);
        }
    } else if !opts.dry_run && !non_atomic_batch.is_empty() {
        let indices: Vec<_> = non_atomic_batch.iter().map(|(idx, _)| *idx).collect();
        let updates: Vec<_> = non_atomic_batch.into_iter().map(|(_, u)| u).collect();
        match crate::refs::commit_ref_updates_non_atomic_on_store(remote_repo.refs(), updates) {
            Ok(outcomes) => {
                for (idx, outcome) in indices.into_iter().zip(outcomes) {
                    if let Err(err) = outcome.result {
                        apply_ref_store_error_to_push_decision(
                            &mut decisions[idx],
                            &Error::RefStore(err),
                            false,
                        );
                    }
                }
            }
            Err(err) => {
                for idx in indices {
                    apply_ref_store_error_to_push_decision(&mut decisions[idx], &err, false);
                }
            }
        }
    }

    if !opts.dry_run {
        let applied: Vec<crate::serve::RefCommand> = decisions
            .iter()
            .filter(|d| d.apply)
            .filter_map(PushDecision::ref_command)
            .collect();
        if !applied.is_empty() {
            let applied: Vec<&crate::serve::RefCommand> = applied.iter().collect();
            relay_hook_output(
                progress,
                &mut remote_messages,
                &scripts.post_receive(&applied),
            );
            relay_hook_output(
                progress,
                &mut remote_messages,
                &scripts.post_update(&applied),
            );
        }
    }

    let results: Vec<_> = decisions.into_iter().map(|d| d.result).collect();
    if !opts.dry_run {
        if let Some(remote) = opts.tracking_remote.as_deref() {
            crate::branch_tracking::apply_push_remote_tracking_updates_in(local, remote, &results)?;
        }
    }

    drop(migrated_pack_keeps);

    Ok(PushOutcome {
        results,
        remote_messages,
    })
}

/// An `atomic` push with a refused update applies none: demote every otherwise-accepted
/// update to [`PushRefStatus::AtomicPushFailed`].
fn fail_atomic_push(decisions: &mut [PushDecision]) {
    for d in decisions {
        if matches!(d.result.status, PushRefStatus::Ok) {
            d.result.status = PushRefStatus::AtomicPushFailed;
            d.apply = false;
        }
    }
}

/// Pass what a receive hook wrote (stdout, then stderr) to `progress` and append it to
/// `messages`, one entry per non-empty line, as a wire push handles the remote's side-band
/// hook output.
fn relay_hook_output(
    progress: &mut dyn crate::fetch::Progress,
    messages: &mut Vec<String>,
    result: &crate::hooks::HookResult,
) {
    use crate::sideband_lines::{RemoteLine, SidebandLines};

    let Some(output) = result.output() else {
        return;
    };
    let mut record = |line: RemoteLine| {
        if let RemoteLine::Line(text) = line {
            if !text.is_empty() {
                messages.push(text);
            }
        }
    };
    for stream in [&output.stdout, &output.stderr] {
        if !stream.is_empty() {
            progress.message(stream);
        }
        let mut lines = SidebandLines::new();
        lines.feed(stream, &mut record);
        lines.finish(&mut record);
    }
}

/// Object database for receive-policy checks during local push.
///
/// Matches wire receive-pack: the pusher's objects are primary and the remote's
/// object store is consulted via alternates so fast-forward walks see both the
/// current remote tip and objects not yet copied. After a real push copies
/// objects, the remote repository odb alone is enough.
fn push_policy_object_lookup(remote_git_dir: &Path, local_git_dir: &Path) -> Odb {
    Odb::new(&objects_dir(local_git_dir)).with_env_alternate_dirs(vec![objects_dir(remote_git_dir)])
}

/// Apply the remote repository's `receive.*` policy to updates that would mutate refs.
///
/// `quarantine` holds the objects a real push copied; a dry run copied none and looks
/// objects up in the pusher's store as well.
fn apply_remote_receive_policy(
    remote_repo: &crate::repo::Repository,
    local_git_dir: &Path,
    dry_run: bool,
    quarantine: Option<&Odb>,
    decisions: &mut [PushDecision],
) -> Result<()> {
    if !decisions.iter().any(|d| d.apply) {
        return Ok(());
    }
    let cfg = remote_repo
        .config()
        .unwrap_or_else(|_| std::sync::Arc::new(crate::config::ConfigSet::new()));
    let policy = crate::serve::ReceivePolicy::from_config(cfg.as_ref());
    let hidden_refs = crate::hide_refs::hide_ref_patterns_receive(cfg.as_ref());
    let head_branch = crate::serve::head_branch(remote_repo);
    let checked_out = crate::serve::checked_out_branch(remote_repo);
    let branches = crate::serve::HeadBranches {
        head: head_branch.as_deref(),
        checked_out: checked_out.as_deref(),
    };
    let pre_copy_lookup =
        dry_run.then(|| push_policy_object_lookup(remote_repo.git_dir(), local_git_dir));
    let object_lookup = pre_copy_lookup
        .as_ref()
        .or(quarantine)
        .unwrap_or(remote_repo.odb());

    for d in decisions.iter_mut().filter(|d| d.apply) {
        let Some(cmd) = d.ref_command() else {
            continue;
        };
        let refusal = match crate::serve::validate_ref_update(
            remote_repo,
            Some(object_lookup),
            &cmd,
            &policy,
            &hidden_refs,
            branches,
        ) {
            Ok(r) => r,
            Err(crate::serve::ServeError::Repository(e)) => return Err(e),
            Err(e) => return Err(Error::Message(e.to_string())),
        };
        if let Some(reason) = refusal {
            d.result.status = match (&reason, cmd.old) {
                (crate::serve::RefUpdateRefusal::StaleInfo, None) => {
                    PushRefStatus::RejectAlreadyExists
                }
                (crate::serve::RefUpdateRefusal::StaleInfo, Some(_)) => PushRefStatus::RejectStale,
                _ => PushRefStatus::RemoteRejected,
            };
            d.result.message = Some(match d.result.status {
                PushRefStatus::RejectAlreadyExists => "already exists".to_owned(),
                _ => reason.to_string(),
            });
            d.apply = false;
            d.action = PushAction::None;
        }
    }
    Ok(())
}

/// The remote ref update for a decided push, compare-and-swapped against `observed_old`.
///
/// Like a receive-pack command, an update applies only if the ref still holds the value the
/// decision saw: `Some(oid)` must still be `oid`, and `None` (a create) must still be absent.
fn push_store_ref_update(
    refname: &str,
    new_oid: Option<ObjectId>,
    observed_old: Option<ObjectId>,
) -> crate::refs::store::RefUpdate {
    crate::refs::store::RefUpdate {
        name: refname.to_owned(),
        new_value: new_oid.map(crate::refs::store::RawRef::Direct),
        expected: match observed_old {
            Some(oid) => crate::refs::store::Expected::Oid(oid),
            None => crate::refs::store::Expected::Missing,
        },
        reflog: None,
        flags: Default::default(),
    }
}

fn push_status_for_ref_store_error(err: &Error) -> PushRefStatus {
    match err {
        Error::RefStore(crate::refs::store::RefStoreError::ExpectedMismatch {
            expected: crate::refs::store::Expected::Missing,
            ..
        }) => PushRefStatus::RejectAlreadyExists,
        Error::RefStore(crate::refs::store::RefStoreError::ExpectedMismatch { .. }) => {
            PushRefStatus::RejectStale
        }
        _ => PushRefStatus::RemoteRejected,
    }
}

/// The per-ref message for a ref store failure while applying a push update.
fn push_message_for_ref_store_error(err: &Error) -> String {
    match push_status_for_ref_store_error(err) {
        PushRefStatus::RejectAlreadyExists => "already exists".to_owned(),
        _ => err.to_string(),
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
        | crate::refs::store::RefStoreError::InvalidSymrefTarget { name, .. }
        | crate::refs::store::RefStoreError::NullOid { name }
        | crate::refs::store::RefStoreError::MissingObject { name, .. }
        | crate::refs::store::RefStoreError::NotACommit { name, .. }
        | crate::refs::store::RefStoreError::ObjectLookup { name, .. }
        | crate::refs::store::RefStoreError::NotFound { name }
        | crate::refs::store::RefStoreError::ReflogRewriteStale { name } => name.clone(),
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
        | crate::refs::store::RefStoreError::Corrupt(_)
        | crate::refs::store::RefStoreError::GitDirPathIo { .. }
        | crate::refs::store::RefStoreError::InvalidReflog(_)
        | crate::refs::store::RefStoreError::Backend(_)
        | crate::refs::store::RefStoreError::Hook(_) => return None,
    })
}

fn apply_ref_store_error_to_push_decision(d: &mut PushDecision, err: &Error, _atomic: bool) {
    d.result.status = push_status_for_ref_store_error(err);
    d.result.message = Some(push_message_for_ref_store_error(err));
    d.apply = false;
}

fn apply_atomic_batch_ref_store_error(
    decisions: &mut [PushDecision],
    batch: &[crate::refs::store::RefUpdate],
    err: Error,
) {
    let fail_ref = ref_store_error_refname(&err);
    let message = push_message_for_ref_store_error(&err);
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

impl PushDecision {
    /// The `<old> <new> <ref>` command a receive-pack would get for this update, or `None`
    /// when it changes nothing.
    fn ref_command(&self) -> Option<crate::serve::RefCommand> {
        let new = match self.action {
            PushAction::Update(src) => Some(src),
            PushAction::Delete => None,
            PushAction::None => return None,
        };
        Some(crate::serve::RefCommand {
            refname: self.result.remote_ref.clone(),
            old: self.result.old_oid,
            new,
        })
    }

    /// Record that the remote refused this update for `reason`; nothing is applied.
    fn refuse(&mut self, reason: &crate::serve::RefUpdateRefusal) {
        self.result.status = PushRefStatus::RemoteRejected;
        self.result.message = Some(reason.to_string());
        self.apply = false;
        self.action = PushAction::None;
    }
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
                        local_ref: push_spec_local_ref(spec),
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
                local_ref: push_spec_local_ref(spec),
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
        let (status, action, apply, message) = match remote_current {
            Some(_) => (PushRefStatus::Ok, PushAction::Delete, true, None),
            None => (
                PushRefStatus::RejectRemoteRefMissing,
                PushAction::None,
                false,
                Some("remote ref does not exist".to_owned()),
            ),
        };
        return Ok(PushDecision {
            result: PushRefResult {
                local_ref: push_spec_local_ref(spec),
                remote_ref: spec.dst.clone(),
                old_oid: remote_current,
                new_oid: None,
                forced: false,
                deletion: true,
                status,
                message,
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
    if !local_odb.contains(&src)? {
        return Err(Error::Message(format!(
            "source object {src} for '{}' is missing from the local object store",
            spec.dst
        )));
    }

    // Unchanged: the remote is already at the source.
    if remote_current == Some(src) {
        return Ok(PushDecision {
            result: PushRefResult {
                local_ref: push_spec_local_ref(spec),
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
                local_ref: push_spec_local_ref(spec),
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
                local_ref: push_spec_local_ref(spec),
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
                local_ref: push_spec_local_ref(spec),
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
                local_ref: push_spec_local_ref(spec),
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
    /// Added by [`TagMode::Following`] rather than selected by a refspec, so
    /// [`crate::fetch::retain_following_tags`] may drop it when its target is not
    /// in the fetched history.
    pub(crate) auto_followed: bool,
    /// Local destination is under `refs/tags/` (tag clobber rules apply).
    pub(crate) dest_is_tag: bool,
    /// Peeled target from ls-refs `peeled:` (annotated tags), when known.
    pub(crate) advertised_peel: Option<ObjectId>,
    /// How this ref appears in [`FETCH_HEAD`](https://git-scm.com/docs/git-fetch#_discussion).
    pub(crate) fetch_head: crate::fetch_request::FetchHeadPolicy,
}

/// Open an [`Odb`] for a git directory, attaching the git dir so `hash_algo`
/// (and MIDX config) resolve correctly.
/// `objects/` of `git_dir`, which lives in the common directory for a linked worktree's
/// admin dir.
fn objects_dir(git_dir: &Path) -> PathBuf {
    crate::repo::common_git_dir_for_config(git_dir).join("objects")
}

pub(crate) fn open_odb(git_dir: &Path) -> Odb {
    Odb::new(&objects_dir(git_dir)).with_config_git_dir(git_dir.to_path_buf())
}

/// Infer the remote repository hash algorithm from fetch capability lines and ref oids.
pub(crate) fn remote_hash_algo_from_advertisement(
    capability_blocks: &[&[String]],
    remote_refs: &[(String, ObjectId)],
) -> HashAlgo {
    for caps in capability_blocks {
        for c in *caps {
            if let Some(name) = c.strip_prefix("object-format=") {
                if let Some(algo) = HashAlgo::from_name(name) {
                    return algo;
                }
            }
        }
    }
    remote_refs
        .iter()
        .find_map(|(_, oid)| HashAlgo::from_len(oid.as_bytes().len()))
        .unwrap_or(HashAlgo::Sha1)
}

/// Before the first pack lands in a fresh clone, align `extensions.objectformat` with the remote.
///
/// Returns whether the local config was rewritten.
pub(crate) fn sync_empty_destination_object_format(
    local_git_dir: &Path,
    remote_algo: HashAlgo,
    opts: &FetchOptions,
) -> Result<bool> {
    if opts.dry_run || !opts.initial_remote_fetch {
        return Ok(false);
    }
    let local_odb = open_odb(local_git_dir);
    if local_odb.hash_algo() == remote_algo {
        return Ok(false);
    }
    if local_odb.approximate_object_count()? > 0 {
        return Err(Error::Message(format!(
            "unsupported object format: destination uses {} but remote uses {}; re-clone into an empty directory with a matching format",
            local_odb.hash_algo().name(),
            remote_algo.name()
        )));
    }
    write_empty_destination_object_format(local_git_dir, remote_algo)?;
    Ok(true)
}

/// Reopen `local` after [`sync_empty_destination_object_format`] rewrote its object format, so
/// its object store uses the new hash algorithm.
pub(crate) fn reopen_after_format_change(
    local: &crate::repo::Repository,
) -> Result<crate::repo::Repository> {
    crate::repo::Repository::open(
        local.git_dir(),
        &RepositoryOptions::with_environment(local.environment().clone()),
    )
}

fn write_empty_destination_object_format(git_dir: &Path, algo: HashAlgo) -> Result<()> {
    use crate::config::{ConfigFile, ConfigScope};
    let config_path = crate::repo::local_config_write_path(git_dir);
    let content = fs::read_to_string(&config_path).unwrap_or_default();
    let mut file = ConfigFile::parse(&config_path, &content, ConfigScope::Local)?;
    if algo == HashAlgo::Sha1 {
        file.unset("extensions.objectformat")?;
        file.set("core.repositoryformatversion", "0")?;
    } else {
        file.set("core.repositoryformatversion", "1")?;
        file.set("extensions.objectformat", algo.name())?;
    }
    file.write()?;
    Ok(())
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
    explicit_refspec_fetch: bool,
    default_branch: Option<&str>,
    remote_refs: &[(String, ObjectId)],
    matched: &[MatchedRef],
) -> Result<()> {
    if opts.dry_run {
        return Ok(());
    }
    let write_fetch_head = opts.remote_name.is_none() || explicit_refspec_fetch;
    if !write_fetch_head {
        return Ok(());
    }
    let source = opts.fetch_source.clone().or_else(|| {
        let name = opts.remote_name.as_deref()?;
        let config = config_for_git_dir(git_dir)?;
        config
            .get(&format!("remote.{name}.url"))
            .filter(|url| !url.trim().is_empty())
    });
    let Some(source) = source.as_deref() else {
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
        .filter(|m| m.fetch_head == crate::fetch_request::FetchHeadPolicy::ForMerge)
        .map(|m| FetchHeadLine {
            oid: m.oid,
            for_merge: true,
            description: crate::fetch_request::fetch_head_description(
                &m.remote_ref,
                m.is_tag,
                source,
            ),
        })
        .collect()
}

/// Tip oid of `HEAD` from a ref advertisement (before `HEAD` is stripped).
#[must_use]
pub(crate) fn remote_head_oid_from_advertised(
    advertised: &[(String, ObjectId)],
) -> Option<ObjectId> {
    advertised
        .iter()
        .find(|(name, _)| name == "HEAD")
        .map(|(_, oid)| *oid)
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

    // For Following, decide reachability on the source ODB using the effective
    // shallow grafts for this fetch. Post-pack pruning via
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
        crate::fetch::reachable_commits(remote_odb, &roots, &targets, shallow_boundaries)?
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
                auto_followed: mode == TagMode::Following,
                dest_is_tag: true,
                advertised_peel: peeled,
                fetch_head: crate::fetch_request::FetchHeadPolicy::ForMerge,
            });
        }
    }
    Ok(following_only)
}

/// Commit tip to use when computing shallow boundaries for one matched ref.
fn shallow_want_commit_tip(remote_odb: &Odb, matched: &MatchedRef) -> Result<ObjectId> {
    if !matched.is_tag {
        return Ok(matched.oid);
    }
    if let Some(peel) = matched.advertised_peel.filter(|p| !p.is_zero()) {
        return Ok(peel);
    }
    if remote_odb.contains(&matched.oid)? {
        let (peeled, _) = remote_odb.peel_tags(matched.oid)?;
        return Ok(peeled);
    }
    Ok(matched.oid)
}

/// Whether a tag ref's peeled target (or the oid itself for lightweight tags)
/// exists in the local or remote object database.
fn ref_target_exists(local_odb: &Odb, remote_odb: &Odb, tag_oid: ObjectId) -> Result<bool> {
    let peel_odb = if local_odb.contains(&tag_oid)? {
        local_odb
    } else {
        remote_odb
    };
    match peel_odb.peel_tags(tag_oid) {
        Ok((peeled, _)) => local_odb.contains(&peeled),
        Err(_) => Ok(false),
    }
}

/// Classify a single ref update into an [`UpdateMode`].
///
/// When `classify_odb` is set (network fetch `--dry-run` after quarantined pack
/// ingest), ancestry uses that store with its configured alternates instead of
/// opening a [`crate::repo::Repository`].
pub(crate) fn classify_update(
    old: Option<&ObjectId>,
    new: &ObjectId,
    force: bool,
    is_tag: bool,
    repo: Option<&crate::repo::Repository>,
    classify_odb: Option<&crate::odb::Odb>,
) -> UpdateMode {
    let Some(old) = old else {
        return UpdateMode::New;
    };
    if old == new {
        return UpdateMode::UpToDate;
    }
    // Fast-forward when old is an ancestor of new (commit history only).
    let ff = if let Some(r) = repo {
        crate::merge_base::is_ancestor(r, *old, *new).unwrap_or(false)
    } else if let Some(odb) = classify_odb {
        crate::merge_base::is_ancestor_odb_walk(odb, *old, *new).unwrap_or(false)
    } else {
        false
    };
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

/// Local tracking refs that would be removed by `git fetch --prune` given the current remote.
///
/// Returns full ref names and their current object ids. The default-branch symref
/// `refs/remotes/<remote>/HEAD` is never included.
///
/// # Errors
///
/// Propagates ref listing failures.
pub fn tracking_refs_gone_on_remote(
    local_git_dir: &Path,
    positive: &[RefspecItem],
    remote_refs: &[(String, ObjectId)],
) -> Result<Vec<(String, ObjectId)>> {
    tracking_refs_gone_with(
        positive,
        remote_refs,
        |prefix| crate::refs::list_refs(local_git_dir, prefix),
        |name| crate::refs::resolve_ref(local_git_dir, name),
    )
}

/// [`tracking_refs_gone_on_remote`] reading the local tracking refs through `local`'s ref
/// store, so an injected [`crate::refs::store::RefStore`] is honoured.
///
/// # Errors
///
/// Propagates ref listing failures.
pub(crate) fn tracking_refs_gone_in(
    local: &crate::repo::Repository,
    positive: &[RefspecItem],
    remote_refs: &[(String, ObjectId)],
) -> Result<Vec<(String, ObjectId)>> {
    tracking_refs_gone_with(
        positive,
        remote_refs,
        |prefix| crate::refs::list_refs_in(local, prefix),
        |name| crate::refs::resolve_ref_in(local, name),
    )
}

fn tracking_refs_gone_with(
    positive: &[RefspecItem],
    remote_refs: &[(String, ObjectId)],
    list: impl Fn(&str) -> Result<Vec<(String, ObjectId)>>,
    resolve: impl Fn(&str) -> Result<ObjectId>,
) -> Result<Vec<(String, ObjectId)>> {
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
            let prefix = &dst[..star];
            for (name, oid) in list(prefix)? {
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
            if let Ok(oid) = resolve(dst) {
                pruned.entry(dst.to_owned()).or_insert(oid);
            }
        }
    }
    Ok(pruned.into_iter().collect())
}

/// Optional prune hint when a fetch ref write is rejected.
pub(crate) struct FetchRefRejectHint<'a> {
    /// Whether this fetch runs with `--prune` (no stale-ref hints when true).
    pub prune: bool,
    /// Positive refspecs for the fetch (stale tracking detection).
    pub positive: &'a [RefspecItem],
    /// Advertised remote refs at fetch time.
    pub remote_refs: &'a [(String, ObjectId)],
    /// Remote nickname (`origin`, …) for hint text.
    pub remote_name: Option<&'a str>,
}

fn fetch_ref_store_error_rejectable(err: &Error) -> bool {
    matches!(
        err,
        Error::RefStore(crate::refs::store::RefStoreError::NameUnavailable { .. })
            | Error::RefStore(crate::refs::store::RefStoreError::LockHeld { .. })
    )
}

fn blocking_ref_from_unavailable(reason: &crate::refs::RefnameUnavailable) -> Option<&str> {
    match reason {
        crate::refs::RefnameUnavailable::AncestorExists { blocking, .. }
        | crate::refs::RefnameUnavailable::DescendantExists { blocking, .. } => Some(blocking),
        crate::refs::RefnameUnavailable::SameBatch { .. } => None,
    }
}

fn append_fetch_prune_hint(
    note: &mut String,
    stale_tracking: &HashSet<String>,
    blocking: &str,
    remote_name: Option<&str>,
) {
    if stale_tracking.contains(blocking) {
        let remote = remote_name.unwrap_or("origin");
        note.push_str(&format!(
            " — stale remote-tracking ref; try `grit fetch --prune` or `grit remote prune {remote}`"
        ));
    }
}

fn mark_fetch_ref_update_rejected(
    updates: &mut [RefUpdate],
    local_ref: &str,
    err: &Error,
    stale_tracking: &HashSet<String>,
    hint: &FetchRefRejectHint<'_>,
) {
    let Some(update) = updates
        .iter_mut()
        .find(|u| u.local_ref.as_deref() == Some(local_ref))
    else {
        return;
    };
    update.mode = UpdateMode::RefLockRejected;
    let mut note = match err {
        Error::RefStore(crate::refs::store::RefStoreError::NameUnavailable { reason }) => {
            let mut text = reason.lock_message_suffix();
            if let Some(blocking) = blocking_ref_from_unavailable(reason) {
                append_fetch_prune_hint(&mut text, stale_tracking, blocking, hint.remote_name);
            }
            text
        }
        Error::RefStore(crate::refs::store::RefStoreError::LockHeld { name }) => {
            format!("ref '{name}' is locked")
        }
        _ => err.to_string(),
    };
    if note.is_empty() {
        note = "unable to update local ref".to_owned();
    }
    update.note = Some(note);
}

/// Commit tracking-ref writes from a fetch into `local`'s ref store, applying every ref
/// that can be updated.
///
/// Each of `writes` stores a value under a name whatever the ref held before, without a
/// reflog entry. They are committed as one batch transaction when the full batch validates;
/// otherwise one at a time, recording lock or namespace failures on the matching
/// [`RefUpdate`] rows instead of aborting the whole fetch.
///
/// With `initial` (the first fetch of a clone), the direct refs are committed first as an
/// initial transaction ([`crate::refs::store::RefTransaction::initial`]), which the files
/// backend stores in one `packed-refs` write; symbolic refs follow as a regular batch.
///
/// # Errors
///
/// Propagates non-rejectable ref-store and I/O errors from the fallback path.
pub(crate) fn apply_fetch_ref_updates(
    local: &crate::repo::Repository,
    writes: Vec<(String, crate::refs::store::RawRef)>,
    updates: &mut [RefUpdate],
    hint: FetchRefRejectHint<'_>,
    initial: bool,
) -> Result<()> {
    use crate::refs::store::RawRef;
    let store_update = |(name, value): &(String, RawRef)| crate::refs::store::RefUpdate {
        name: name.clone(),
        new_value: Some(value.clone()),
        expected: crate::refs::store::Expected::Any,
        reflog: None,
        flags: Default::default(),
    };
    let store = local.refs();
    let mut pending: Vec<&(String, RawRef)> = writes.iter().collect();
    if initial {
        let (direct, rest): (Vec<_>, Vec<_>) = pending
            .iter()
            .copied()
            .partition(|(_, value)| matches!(value, RawRef::Direct(_)));
        let direct = direct.into_iter().map(store_update).collect();
        if crate::refs::commit_initial_ref_updates_on_store(store, direct).is_ok() {
            pending = rest;
        }
    }
    let batch = pending.iter().copied().map(store_update).collect();
    if pending.is_empty() || crate::refs::commit_owned_ref_updates_on_store(store, batch).is_ok() {
        return Ok(());
    }

    let stale_tracking: HashSet<String> = if hint.prune {
        HashSet::new()
    } else {
        tracking_refs_gone_in(local, hint.positive, hint.remote_refs)?
            .into_iter()
            .map(|(name, _)| name)
            .collect()
    };

    for write in pending {
        match crate::refs::commit_ref_update_on_store(store, store_update(write)) {
            Ok(()) => {}
            Err(err) if fetch_ref_store_error_rejectable(&err) => {
                mark_fetch_ref_update_rejected(updates, &write.0, &err, &stale_tracking, &hint);
            }
            Err(err) => return Err(err),
        }
    }
    Ok(())
}

/// Delete local tracking refs whose remote counterpart no longer exists.
///
/// A local tracking ref is a prune candidate when it lives under the destination
/// namespace of some positive wildcard refspec and no current remote ref maps to
/// it. Matches `git fetch --prune` for the common `refs/remotes/<remote>/*` case.
///
/// Candidates are read from, and (unless `dry_run`) deleted in one batch through, `local`'s
/// ref store. Each pruned ref is reported in `updates`.
///
/// # Errors
///
/// Propagates ref listing and ref-store failures.
pub(crate) fn prune_tracking_refs(
    local: &crate::repo::Repository,
    positive: &[RefspecItem],
    remote_refs: &[(String, ObjectId)],
    dry_run: bool,
    updates: &mut Vec<RefUpdate>,
) -> Result<()> {
    let pruned: Vec<(String, ObjectId)> = tracking_refs_gone_in(local, positive, remote_refs)?;

    let prune_delete_flags = crate::refs::store::RefUpdateFlags {
        no_deref: true,
        ..Default::default()
    };
    let mut batch: Vec<crate::refs::store::RefUpdate> = Vec::new();
    for (name, oid) in pruned.into_iter() {
        if !dry_run {
            batch.push(crate::refs::store::RefUpdate {
                name: name.clone(),
                new_value: None,
                expected: crate::refs::store::Expected::Any,
                reflog: None,
                flags: prune_delete_flags,
            });
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
    crate::refs::commit_owned_ref_updates_on_store(local.refs(), batch)
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
        copying_counts: Vec<u64>,
        active: Option<ProgressPhase>,
        current: u64,
        received: Vec<ReceivedObjects>,
        want_received: bool,
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
            self.record_count();
        }
        fn set(&mut self, current: u64) {
            self.events.push("set");
            self.current = current;
            self.record_count();
        }
        fn finish(&mut self) {
            self.events.push("finish");
            if let Some(phase) = self.active.take() {
                self.finished.push(phase);
            }
        }
    }

    impl PhaseRecorder {
        fn record_count(&mut self) {
            self.counts.push(self.current);
            if self.active == Some(ProgressPhase::CopyingObjects) {
                self.copying_counts.push(self.current);
            }
        }
    }

    impl Progress for PhaseRecorder {
        fn wants_received_objects(&self) -> bool {
            self.want_received
        }

        fn received_objects(&mut self, counts: ReceivedObjects) {
            self.received.push(counts);
        }
    }

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

    /// Where in [`push_local_from`] a test hook interleaves another writer.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub(crate) enum PushHookPoint {
        /// After every update is decided and its objects copied, before the remote's
        /// `receive.*` policy rechecks each update against the remote refs.
        BeforeReceivePolicy,
        /// After the policy check, before any remote ref is written.
        BeforeRefCommit,
    }

    type PushHook = (PushHookPoint, Box<dyn FnOnce()>);

    // hygiene: test-only push interleaving hook
    thread_local! {
        static PUSH_HOOK: std::cell::RefCell<Option<PushHook>> =
            const { std::cell::RefCell::new(None) };
    }

    /// Runs (once) the hook installed by [`on_next_push`] when the push reaches `point`.
    pub(super) fn run_push_hook(point: PushHookPoint) {
        let hook = PUSH_HOOK.with(|slot| {
            let mut slot = slot.borrow_mut();
            match slot.as_ref() {
                Some((at, _)) if *at == point => slot.take().map(|(_, hook)| hook),
                _ => None,
            }
        });
        if let Some(hook) = hook {
            hook();
        }
    }

    /// Run `hook` inside the next local push on this thread when it reaches `point`.
    pub(crate) fn on_next_push(point: PushHookPoint, hook: impl FnOnce() + 'static) {
        PUSH_HOOK.with(|slot| *slot.borrow_mut() = Some((point, Box::new(hook))));
    }

    fn git_stdout(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git") // hygiene: git fixture comparison in cfg(test) module
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.com")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.com")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .current_dir(dir)
            .args(args)
            .output()
            .expect("run git");
        assert!(out.status.success(), "git {args:?} failed");
        String::from_utf8(out.stdout)
            .expect("utf8")
            .trim()
            .to_owned()
    }

    /// Fixture: a bare remote with `main`, plus a clone holding one more commit on `main` and
    /// a second, diverging commit already stored in the remote as `refs/heads/other`.
    /// Returns `(remote, local_git_dir, local_tip, other_tip)`.
    fn racing_create_fixture(tmp: &Path) -> (PathBuf, PathBuf, ObjectId, ObjectId) {
        let remote = tmp.join("remote.git");
        let local = tmp.join("local");
        std::fs::create_dir_all(&remote).unwrap();
        std::fs::create_dir_all(&local).unwrap();
        assert!(git_ok(&remote, &["init", "-q", "--bare", "-b", "main"]));
        assert!(git_ok(&local, &["init", "-q", "-b", "main"]));
        assert!(git_ok(&local, &["commit", "--allow-empty", "-qm", "base"]));
        assert!(git_ok(
            &local,
            &["push", "-q", remote.to_str().unwrap(), "main"]
        ));
        assert!(git_ok(&local, &["checkout", "-q", "-b", "other"]));
        assert!(git_ok(&local, &["commit", "--allow-empty", "-qm", "other"]));
        assert!(git_ok(
            &local,
            &["push", "-q", remote.to_str().unwrap(), "other"]
        ));
        let other = ObjectId::from_hex(&git_stdout(&local, &["rev-parse", "HEAD"])).unwrap();
        assert!(git_ok(&local, &["checkout", "-q", "main"]));
        assert!(git_ok(&local, &["commit", "--allow-empty", "-qm", "mine"]));
        let tip = ObjectId::from_hex(&git_stdout(&local, &["rev-parse", "HEAD"])).unwrap();
        (remote, local.join(".git"), tip, other)
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn push_local_create_rejects_ref_created_after_decision() {
        let points = [
            PushHookPoint::BeforeReceivePolicy,
            PushHookPoint::BeforeRefCommit,
        ];
        let modes = [(false, false), (true, false), (false, true)];
        for (point, (force, atomic)) in points
            .into_iter()
            .flat_map(|p| modes.into_iter().map(move |m| (p, m)))
        {
            let tmp = tempfile::tempdir().unwrap();
            let (remote, local_git, tip, other) = racing_create_fixture(tmp.path());

            let racer_remote = remote.clone();
            on_next_push(point, move || {
                assert!(git_ok(
                    &racer_remote,
                    &["update-ref", "refs/heads/shared", &other.to_hex(), ""]
                ));
            });
            let outcome = push_local(
                &local_git,
                &remote,
                &[PushRefSpec {
                    src: Some(tip),
                    dst: "refs/heads/shared".to_owned(),
                    force,
                    ..Default::default()
                }],
                &PushOptions {
                    atomic,
                    ..Default::default()
                },
                &crate::progress::NeverCancel,
            )
            .expect("push call");

            let result = &outcome.results[0];
            let case = format!("{point:?} force={force} atomic={atomic}");
            assert_eq!(result.status, PushRefStatus::RejectAlreadyExists, "{case}");
            assert!(result.status.is_error(), "{case}");
            assert_eq!(result.message.as_deref(), Some("already exists"), "{case}");
            assert_eq!(
                git_stdout(&remote, &["rev-parse", "refs/heads/shared"]),
                other.to_hex(),
                "{case}: the racing creator's value must survive"
            );
            assert!(!git_ok(
                &remote,
                &[
                    "merge-base",
                    "--is-ancestor",
                    &tip.to_hex(),
                    &other.to_hex()
                ]
            ));
        }
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn push_local_update_rejects_ref_moved_after_decision() {
        for point in [
            PushHookPoint::BeforeReceivePolicy,
            PushHookPoint::BeforeRefCommit,
        ] {
            let tmp = tempfile::tempdir().unwrap();
            let (remote, local_git, tip, other) = racing_create_fixture(tmp.path());

            let racer_remote = remote.clone();
            on_next_push(point, move || {
                assert!(git_ok(
                    &racer_remote,
                    &["update-ref", "refs/heads/main", &other.to_hex()]
                ));
            });
            let outcome = push_local(
                &local_git,
                &remote,
                &[PushRefSpec {
                    src: Some(tip),
                    dst: "refs/heads/main".to_owned(),
                    ..Default::default()
                }],
                &PushOptions::default(),
                &crate::progress::NeverCancel,
            )
            .expect("push call");

            assert_eq!(
                outcome.results[0].status,
                PushRefStatus::RejectStale,
                "{point:?}"
            );
            assert_eq!(
                git_stdout(&remote, &["rev-parse", "refs/heads/main"]),
                other.to_hex(),
                "{point:?}"
            );
        }
    }

    fn assert_monotonic(counts: &[u64]) {
        for pair in counts.windows(2) {
            assert!(pair[1] >= pair[0], "progress counts must be monotonic");
        }
    }

    fn copying_phase_total(rec: &PhaseRecorder) -> Option<u64> {
        rec.started
            .iter()
            .find(|(phase, _)| *phase == ProgressPhase::CopyingObjects)
            .and_then(|(_, total)| *total)
    }

    fn assert_copy_progress_within_total(rec: &PhaseRecorder, copied_objects: u64) {
        let Some(total) = copying_phase_total(rec) else {
            panic!("missing CopyingObjects start: {:?}", rec.started);
        };
        assert!(
            total >= copied_objects,
            "progress total {total} smaller than copied objects {copied_objects}"
        );
        for &count in &rec.copying_counts {
            assert!(
                count <= total,
                "copy progress {count} exceeds phase total {total}"
            );
        }
        for snapshot in &rec.received {
            assert!(
                snapshot.received <= snapshot.total,
                "received {} exceeds snapshot total {}",
                snapshot.received,
                snapshot.total
            );
            assert!(
                snapshot.total <= total,
                "snapshot total {} exceeds phase total {}",
                snapshot.total,
                total
            );
            assert!(
                snapshot.received <= total,
                "received {} exceeds phase total {}",
                snapshot.received,
                total
            );
        }
        assert_eq!(
            rec.copying_counts.last().copied().unwrap_or(0),
            copied_objects,
            "final copy count should match transferred objects"
        );
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn local_fetch_classifies_through_repository_ref_store() {
        use crate::refs::store::{MemoryRefStore, RawRef};
        use std::sync::Arc;

        let tmp = tempfile::tempdir().unwrap();
        assert!(git_ok(tmp.path(), &["init", "-q", "-b", "main"]));
        assert!(git_ok(
            tmp.path(),
            &["commit", "--allow-empty", "-qm", "base"]
        ));
        let repo =
            crate::repo::Repository::open(&tmp.path().join(".git"), &RepositoryOptions::empty())
                .unwrap();
        let tip = repo.resolve_ref_oid("HEAD").unwrap();
        let store = Arc::new(MemoryRefStore::new());
        store
            .set_ref("refs/remotes/origin/main", RawRef::Direct(tip))
            .unwrap();
        let repo = repo.with_ref_store(store);
        let opts = FetchOptions {
            refspecs: vec!["+refs/heads/*:refs/remotes/origin/*".into()],
            tags: TagMode::None,
            dry_run: true,
            ..Default::default()
        };
        let outcome = fetch_local_into(
            &repo,
            &tmp.path().join(".git"),
            &opts,
            &mut PhaseRecorder::default(),
            &crate::progress::NeverCancel,
        )
        .unwrap();
        assert_eq!(outcome.transfer.objects, 0);
        assert_eq!(outcome.updates.len(), 1);
        assert_eq!(outcome.updates[0].old_oid, Some(tip));
        assert_eq!(outcome.updates[0].mode, UpdateMode::UpToDate);
    }

    fn write_linear_commits(objects: &Path, depth: usize) -> (Odb, ObjectId) {
        use crate::objects::{serialize_commit, CommitData};
        let odb = Odb::new(objects);
        let mut parent = None;
        for i in 0..depth {
            let blob = odb
                .write(ObjectKind::Blob, format!("payload {i}\n").as_bytes())
                .expect("blob");
            let mut tree = b"100644 f.txt\0".to_vec();
            tree.extend_from_slice(blob.as_bytes());
            let tree_oid = odb.write(ObjectKind::Tree, &tree).expect("tree");
            let commit = CommitData {
                tree: tree_oid,
                parents: parent.into_iter().collect(),
                author: "T <t@e.com> 0 +0000".into(),
                committer: "T <t@e.com> 0 +0000".into(),
                author_raw: Vec::new(),
                committer_raw: Vec::new(),
                encoding: None,
                message: format!("c{i}\n"),
                raw_message: None,
                extra_headers: Vec::new(),
            };
            parent = Some(
                odb.write(ObjectKind::Commit, &serialize_commit(&commit))
                    .expect("commit"),
            );
        }
        (odb, parent.expect("tip"))
    }

    #[test]
    fn fetch_local_hardlinked_pack_reports_scan_progress() {
        let remote = tempfile::tempdir().expect("remote");
        assert!(git_ok(
            remote.path(),
            &["init", "-q", "-b", "main", "--bare"]
        ));
        let src = tempfile::tempdir().expect("src");
        assert!(git_ok(src.path(), &["init", "-q", "-b", "main"]));
        for i in 0..280 {
            std::fs::write(src.path().join("f.txt"), format!("payload {i}\n")).unwrap();
            assert!(git_ok(src.path(), &["add", "f.txt"]));
            assert!(git_ok(src.path(), &["commit", "-qm", &format!("c{i}")],));
        }
        assert!(git_ok(
            src.path(),
            &["push", "-q", remote.path().to_str().unwrap(), "main:main"],
        ));
        assert!(git_ok(remote.path(), &["repack", "-a", "-d", "-f"],));

        let local = tempfile::tempdir().expect("local");
        init_repository(local.path(), false, "main", None, RefStorageFormat::Files).expect("init");

        let opts = FetchOptions {
            refspecs: vec!["+refs/heads/*:refs/remotes/origin/*".to_owned()],
            initial_remote_fetch: true,
            tags: TagMode::None,
            ..Default::default()
        };
        let mut rec = PhaseRecorder {
            want_received: true,
            ..Default::default()
        };
        let outcome = fetch_local_with_progress(
            &local.path().join(".git"),
            remote.path(),
            &opts,
            &mut rec,
            &crate::progress::NeverCancel,
        )
        .expect("fetch_local");
        assert!(
            outcome.transfer.objects > 500,
            "expected a large hardlinked pack transfer"
        );
        assert_eq!(
            rec.started
                .iter()
                .filter(|(p, _)| *p == ProgressPhase::CopyingObjects)
                .count(),
            1,
            "expected a single CopyingObjects phase, got {:?}",
            rec.started
        );
        assert!(
            rec.copying_counts.len() >= 3,
            "expected scan progress while checking pack indexes, got {:?}",
            rec.copying_counts
        );
        assert_monotonic(&rec.copying_counts);
    }

    #[test]
    fn fetch_local_packed_remote_reports_incremental_copy_progress() {
        let remote = tempfile::tempdir().expect("remote");
        init_repository(remote.path(), true, "main", None, RefStorageFormat::Files)
            .expect("remote init");
        let objects = remote.path().join("objects");
        let (remote_odb, tip) = write_linear_commits(&objects, 280);
        crate::refs::write_ref(remote.path(), "refs/heads/main", &tip).expect("main ref");
        remote_odb
            .write(ObjectKind::Blob, b"unreachable orphan blob\n")
            .expect("orphan");

        let local = tempfile::tempdir().expect("local");
        init_repository(local.path(), false, "main", None, RefStorageFormat::Files).expect("init");

        let opts = FetchOptions {
            refspecs: vec!["+refs/heads/*:refs/remotes/origin/*".to_owned()],
            initial_remote_fetch: true,
            tags: TagMode::None,
            ..Default::default()
        };
        let mut rec = PhaseRecorder {
            want_received: true,
            ..Default::default()
        };
        let outcome = fetch_local_with_progress(
            &local.path().join(".git"),
            remote.path(),
            &opts,
            &mut rec,
            &crate::progress::NeverCancel,
        )
        .expect("fetch_local");
        assert!(
            !rec.received.is_empty(),
            "expected received_objects callbacks"
        );
        for snapshot in &rec.received {
            assert!(snapshot.received <= snapshot.total);
        }

        let enum_pos = rec
            .started
            .iter()
            .position(|(p, _)| *p == ProgressPhase::EnumeratingObjects)
            .expect("EnumeratingObjects");
        let copy_pos = rec
            .started
            .iter()
            .position(|(p, _)| *p == ProgressPhase::CopyingObjects)
            .expect("CopyingObjects");
        assert!(
            enum_pos < copy_pos,
            "enumerate before copy: {:?}",
            rec.started
        );
        assert_eq!(
            rec.started
                .iter()
                .filter(|(p, _)| *p == ProgressPhase::CopyingObjects)
                .count(),
            1,
            "expected a single CopyingObjects phase, got {:?}",
            rec.started
        );
        assert!(
            rec.counts.len() >= 3,
            "expected >=3 progress samples overall, got {:?}",
            rec.counts
        );
        assert!(
            rec.copying_counts.len() >= 3,
            "expected >=3 copy progress samples, got {:?}",
            rec.copying_counts
        );
        assert_monotonic(&rec.copying_counts);
        assert!(
            rec.copying_counts.last().copied().unwrap_or(0) >= outcome.transfer.objects as u64,
            "final copy count should reach transferred objects"
        );
    }

    #[test]
    fn link_source_objects_progress_counts_objects_not_sidecar_files() {
        for want_received in [false, true] {
            let remote = tempfile::tempdir().expect("remote");
            assert!(git_ok(
                remote.path(),
                &["init", "-q", "-b", "main", "--bare"]
            ));
            let src = tempfile::tempdir().expect("src");
            assert!(git_ok(src.path(), &["init", "-q", "-b", "main"]));
            for i in 0..20 {
                std::fs::write(src.path().join("f.txt"), format!("payload {i}\n")).unwrap();
                assert!(git_ok(src.path(), &["add", "f.txt"]));
                assert!(git_ok(src.path(), &["commit", "-qm", &format!("c{i}")],));
            }
            assert!(git_ok(
                src.path(),
                &["push", "-q", remote.path().to_str().unwrap(), "main:main"],
            ));
            assert!(git_ok(remote.path(), &["repack", "-a", "-d", "-f"],));

            let local = tempfile::tempdir().expect("local");
            init_repository(local.path(), false, "main", None, RefStorageFormat::Files)
                .expect("init");

            let opts = FetchOptions {
                refspecs: vec!["+refs/heads/*:refs/remotes/origin/*".to_owned()],
                initial_remote_fetch: true,
                link_source_objects: true,
                tags: TagMode::None,
                ..Default::default()
            };
            let mut rec = PhaseRecorder {
                want_received,
                ..Default::default()
            };
            let outcome = fetch_local_with_progress(
                &local.path().join(".git"),
                remote.path(),
                &opts,
                &mut rec,
                &crate::progress::NeverCancel,
            )
            .expect("fetch_local");

            if want_received {
                assert!(
                    !rec.received.is_empty(),
                    "expected received_objects when enabled"
                );
            } else {
                assert!(
                    rec.received.is_empty(),
                    "expected no received_objects when disabled"
                );
            }
            assert_copy_progress_within_total(&rec, outcome.transfer.objects as u64);
            assert_ne!(
                copying_phase_total(&rec),
                Some(1),
                "total must be object count, not ref count: {:?}",
                rec.started
            );
        }
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
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

        let opts = FetchOptions {
            refspecs: vec!["+refs/heads/*:refs/remotes/origin/*".to_owned()],
            initial_remote_fetch: true,
            tags: TagMode::None,
            ..Default::default()
        };
        let mut rec = PhaseRecorder::default();
        fetch_local_with_progress(
            &local.path().join(".git"),
            remote.path(),
            &opts,
            &mut rec,
            &crate::progress::NeverCancel,
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
    fn transfer_stats_add_up_and_keep_kinds_only_when_known() {
        use crate::index_pack::ObjectKindCounts;
        let kinds = |commits, blobs| ObjectKindCounts {
            commits,
            trees: 0,
            blobs,
            tags: 0,
        };
        let mut total = TransferStats::default();
        total += TransferStats {
            objects: 3,
            bytes: 100,
            kinds: Some(kinds(1, 2)),
        };
        // An up-to-date remote adds nothing and does not erase the breakdown.
        total += TransferStats::default();
        total += TransferStats {
            objects: 2,
            bytes: 50,
            kinds: Some(kinds(1, 1)),
        };
        assert_eq!(
            total,
            TransferStats {
                objects: 5,
                bytes: 150,
                kinds: Some(kinds(2, 3)),
            }
        );

        // A transfer without a breakdown makes the combined breakdown unknown.
        total += TransferStats {
            objects: 4,
            bytes: 10,
            kinds: None,
        };
        assert_eq!(total.objects, 9);
        assert_eq!(total.bytes, 160);
        assert_eq!(total.kinds, None);
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn transfer_stats_for_local_operation_counts_new_pack_index() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        assert!(git_ok(&repo, &["init", "-q", "-b", "main", "."]));
        std::fs::write(repo.join("blob.txt"), b"payload\n").unwrap();
        assert!(git_ok(&repo, &["add", "blob.txt"]));
        assert!(git_ok(&repo, &["commit", "-qm", "c"]));
        assert!(git_ok(&repo, &["repack", "-a", "-d", "-f"]));

        let git_dir = repo.join(".git");
        let pack_dir = git_dir.join("objects/pack");
        let odb = Odb::new(&git_dir.join("objects")).with_config_git_dir(git_dir.clone());
        let stats =
            transfer_stats_for_local_operation(&odb, &HashSet::new(), None, &HashSet::new())
                .expect("stats");
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

    #[test]
    fn update_mode_report_classification_covers_every_mode() {
        use UpdateMode::*;
        for (mode, reported, rejected) in [
            (New, true, false),
            (FastForward, true, false),
            (Forced, true, false),
            (DeletedMissing, true, false),
            (NonFastForwardRejected, true, true),
            (TagUpdateRejected, true, true),
            (SourceObjectNotFound, true, true),
            (UpToDate, false, false),
            (NoChangeNeeded, false, false),
            (Unborn, false, false),
        ] {
            assert_eq!(mode.is_reported(), reported, "{mode:?}");
            assert_eq!(mode.is_rejected(), rejected, "{mode:?}");
            assert!(!(mode.modified_repo() && mode.is_rejected()), "{mode:?}");
        }
    }

    #[test]
    fn fetch_outcome_summarizes_reported_changed_and_rejected_refs() {
        let update = |name: &str, mode| RefUpdate {
            remote_ref: format!("refs/heads/{name}"),
            local_ref: Some(format!("refs/remotes/origin/{name}")),
            old_oid: None,
            new_oid: None,
            mode,
            note: None,
        };
        let outcome = FetchOutcome {
            updates: vec![
                update("same", UpdateMode::UpToDate),
                update("new", UpdateMode::New),
                update("ff", UpdateMode::FastForward),
                update("noop", UpdateMode::NoChangeNeeded),
            ],
            ..Default::default()
        };
        let reported: Vec<&str> = outcome
            .reported_updates()
            .map(|u| u.remote_ref.as_str())
            .collect();
        assert_eq!(reported, ["refs/heads/new", "refs/heads/ff"]);
        assert_eq!(outcome.changed_ref_count(), 2);
        assert!(!outcome.has_rejections());

        let rejected = FetchOutcome {
            updates: vec![update("diverged", UpdateMode::NonFastForwardRejected)],
            ..Default::default()
        };
        assert_eq!(rejected.changed_ref_count(), 0);
        assert!(rejected.has_rejections());
        assert_eq!(rejected.reported_updates().count(), 1);
        assert!(FetchOutcome::default().reported_updates().next().is_none());
    }
}
