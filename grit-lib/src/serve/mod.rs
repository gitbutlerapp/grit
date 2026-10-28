//! Server side of the Git wire protocol: answering fetches and accepting pushes.
//!
//! This module lets a process act as the remote end of a clone, fetch, or push.
//! [`upload_pack`] serves objects to a fetching client (protocol v0, v1, and v2)
//! and [`receive_pack`] accepts a pushed pack plus ref updates. Both operate on
//! plain byte streams, so the same code serves a duplex connection (ssh, a
//! local pipe) and the stateless request/response exchanges of smart HTTP.
//!
//! A push server decides what to accept with a [`ReceivePolicy`] (the
//! `receive.*` settings), the repository's hook scripts, and optionally an
//! in-process [`ReceiveHook`] passed to [`receive_pack_with_hook`]. Refusals
//! come back typed, as [`RefUpdateRefusal`] per ref and [`UnpackError`] for the
//! pack.
//!
//! The functions take an already-opened [`Repository`] and explicit options; they
//! never read the process environment or print to the terminal. Callers are
//! expected to map `GIT_PROTOCOL` to a [`ProtocolVersion`] and to load
//! configuration at their own boundary.

mod object_info;
mod promised_objects;
mod receive_pack;
mod ref_update_policy;
mod shallow_upload;
mod upload_pack;
mod upload_pack_policy;

pub(crate) use receive_pack::ReceiveHookScripts;
pub(crate) use shallow_upload::{compute_shallow_response, ShallowRequest};

pub use receive_pack::{
    receive_pack, receive_pack_with_hook, receive_pack_with_hook_cancellable, HookRunRecord,
    ReceiveHook, ReceiveHookContext, ReceiveOutcome, ReceivePolicy, RefCommand, RefUpdateRefusal,
    RefUpdateResult, RejectedObject, UnpackError,
};
pub use ref_update_policy::{checked_out_branch, head_branch, validate_ref_update, HeadBranches};
pub use upload_pack::{upload_pack, upload_pack_cancellable, UploadOutcome};

use std::ops::ControlFlow;

use crate::objects::ObjectId;
use crate::refs::listing::PACKED_PEELS_MIN_REFS;
use crate::refs::store::RawRef;
use crate::refs::PackedRefPeels;
use crate::repo::Repository;

/// Errors produced while serving a fetch or a push.
#[derive(Debug, thiserror::Error)]
pub enum ServeError {
    /// The peer sent something that does not follow the wire protocol.
    #[error("protocol error: {0}")]
    Protocol(String),
    /// The client asked for an object that is not an advertised ref tip.
    #[error("not our ref: {0}")]
    NotOurRef(ObjectId),
    /// An object the response needs is not stored in the served repository.
    ///
    /// Only raised before any pack byte is written, so the client gets a clean
    /// `ERR` line (protocol v2) or a side-band error (v0/v1) instead of a pack cut
    /// short. Repositories without promisor remotes report missing objects the
    /// way they always did, as a [`ServeError::Repository`] error.
    #[error("object {oid} is missing: {reason}")]
    ObjectMissing {
        /// The missing object.
        oid: ObjectId,
        /// Why it could not be sent.
        reason: MissingObjectReason,
    },
    /// The client requested a v2 command this server does not implement.
    #[error("unknown command: {0}")]
    UnknownCommand(String),
    /// A `deepen-not` argument (`--shallow-exclude`) cannot be used as a
    /// shallow-history exclusion point.
    #[error("invalid deepen-not '{spec}': {reason}")]
    InvalidDeepenNot {
        /// The argument exactly as the client sent it.
        spec: String,
        /// Why it was rejected.
        reason: DeepenNotError,
    },
    /// An upload-pack filter request was rejected by repository config.
    #[error(transparent)]
    UploadFilter(#[from] crate::upload_filter::UploadFilterError),
    /// The client and repository disagree on the object hash algorithm.
    #[error("object format mismatch: client wants {client}, repository uses {repository}")]
    ObjectFormatMismatch {
        /// Hash algorithm named by the client.
        client: String,
        /// Hash algorithm of the served repository.
        repository: &'static str,
    },
    /// Reading or writing repository data failed.
    #[error(transparent)]
    Repository(#[from] crate::error::Error),
    /// The underlying stream failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// The session was stopped by a cancel signal before finishing.
    #[error("cancelled")]
    Cancelled,
    /// Sending the pack failed after part of it had gone out, so the response
    /// cannot be turned into a clean error any more. With a side band the
    /// client was told on band 3; the transport should end the response as
    /// failed rather than complete it.
    #[error("the pack failed partway through: {0}")]
    PackInterrupted(Box<ServeError>),
}

/// Why a missing object could not be served; see [`ServeError::ObjectMissing`].
///
/// A repository whose large blobs live on a large-object promisor (LOP) leaves
/// them out of its own packs. Such a blob is *promised*: some object in a promisor
/// pack refers to it. A client that accepted one of the advertised promisor
/// remotes is sent the pack without it and fetches it from the LOP itself; any
/// other client needs the server to fetch it first (lazy fetch, see
/// [`ServeOptions::lazy_fetch`]) or gets this error.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum MissingObjectReason {
    /// The object can be fetched from a promisor remote, but the client accepted
    /// none of them and lazy fetch is off.
    #[error("it is stored on a promisor remote the client did not accept")]
    Promised,
    /// No promisor object refers to it: the repository is missing an object it
    /// should have.
    #[error("no promisor remote has it either")]
    NotPromised,
    /// Lazy fetch was requested, but the [`Repository`] was opened without a
    /// [`PromisorFetcher`](crate::promisor_fetch::PromisorFetcher).
    #[error("lazy fetch is on but no promisor fetcher is installed")]
    NoFetcher,
    /// Lazy fetch ran and the object is still not stored.
    #[error("fetching it from the promisor remotes did not bring it in")]
    LazyFetchFailed,
}

/// Why a `deepen-not` argument was rejected; see [`ServeError::InvalidDeepenNot`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum DeepenNotError {
    /// The argument names neither an existing ref nor an object in the repository.
    #[error("no such ref or object")]
    UnknownRef,
    /// A short name matched more than one ref under the ref lookup rules.
    #[error("matches more than one ref")]
    AmbiguousRef,
    /// The ref or object id does not lead to a commit once tags are peeled.
    #[error("does not point at a commit")]
    NotACommit,
}

/// Result alias for serving operations.
pub type Result<T> = std::result::Result<T, ServeError>;

/// Wire protocol version requested by the client.
///
/// Version 1 is version 0 with an explicit `version 1` line in front of the
/// advertisement. Version 2 replaces the ref advertisement with a capability
/// list and command requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ProtocolVersion {
    /// The original protocol, used when the client asks for nothing else.
    #[default]
    V0,
    /// Version 0 framing announced with a `version 1` line.
    V1,
    /// Command-based protocol version 2.
    V2,
}

impl ProtocolVersion {
    /// Pick the highest version named in a `GIT_PROTOCOL` style value.
    ///
    /// The value is a colon-separated list of `key=value` entries; only
    /// `version=N` entries are considered. Unknown or missing versions fall
    /// back to [`ProtocolVersion::V0`].
    ///
    /// # Parameters
    ///
    /// - `value`: the raw value, for example `version=2` or `version=2:foo=bar`.
    ///
    /// # Returns
    ///
    /// The highest supported version the value asks for.
    #[must_use]
    pub fn from_git_protocol(value: &str) -> Self {
        value
            .split(':')
            .filter_map(|entry| entry.strip_prefix("version="))
            .filter_map(|v| match v {
                "2" => Some(Self::V2),
                "1" => Some(Self::V1),
                "0" => Some(Self::V0),
                _ => None,
            })
            .max_by_key(|v| match v {
                Self::V0 => 0,
                Self::V1 => 1,
                Self::V2 => 2,
            })
            .unwrap_or_default()
    }
}

/// How a serving session is framed on the wire.
#[derive(Debug, Clone, Default)]
pub struct ServeOptions {
    /// Protocol version the client asked for.
    pub protocol: ProtocolVersion,
    /// Handle exactly one request from the input and stop, without printing the
    /// advertisement first. Smart HTTP uses this for its POST requests.
    pub stateless_rpc: bool,
    /// Print the ref (or capability) advertisement and stop. Smart HTTP uses
    /// this to answer `GET info/refs`.
    pub advertise_refs: bool,
    /// Value for the `agent=` capability, such as `grit/0.5.0`.
    pub agent: String,
    /// Ref patterns hidden from the advertisement (`transfer.hideRefs` and the
    /// service-specific `uploadpack.hideRefs` / `receive.hideRefs`).
    pub hidden_refs: Vec<String>,
    /// Values the client sent as v2 `server-option=<value>` capability lines on
    /// the current command request. Cleared at the start of each v2 command and
    /// filled before `ls-refs` / `fetch` / `object-info` handlers run.
    pub server_options: Vec<String>,
    /// Bundles to offer through the protocol v2 `bundle-uri` command. `None`
    /// leaves the command out of the advertisement.
    pub bundle_list: Option<crate::bundle_uri::BundleList>,
    /// Where clients may upload a resumable push bundle, advertised by
    /// receive-pack as `pushBundle=<value>`; relative values are relative to
    /// the repository URL. `None` advertises nothing.
    pub push_bundle: Option<String>,
    /// Filled while serving: the promisor remotes the client accepted, from the
    /// `promisor-remote=<names>` capability of its v2 command requests. Only names
    /// this server advertised (see
    /// [`PromisorAdvertisement`](crate::promisor_remote::PromisorAdvertisement))
    /// are kept. The list lasts for the whole session, so an acceptance sent with
    /// `ls-refs` still holds for the `fetch` that follows; a stateless (smart
    /// HTTP) request only sees what that one request carried. Callers leave it
    /// empty.
    pub accepted_promisor_remotes: Vec<String>,
    /// Fetch objects this repository is missing from its own promisor remotes
    /// before answering a client that accepted none of them, so the client still
    /// gets a complete pack. `None` uses `uploadpack.lazyFetch` (a grit setting,
    /// false by default).
    ///
    /// Git's upload-pack turns lazy fetch off unless `GIT_NO_LAZY_FETCH=0` is in
    /// its environment; the library reads no environment, so callers map that
    /// variable here. Fetching needs a
    /// [`PromisorFetcher`](crate::promisor_fetch::PromisorFetcher) on the
    /// [`Repository`], otherwise the request fails with
    /// [`MissingObjectReason::NoFetcher`].
    pub lazy_fetch: Option<bool>,
}

impl ServeOptions {
    /// The [`Self::lazy_fetch`] an upload-pack process should use under `env`, from
    /// `GIT_NO_LAZY_FETCH` (or grit's `GRIT_NO_LAZY_FETCH`, which wins when both are
    /// set): a false value turns lazy fetch on, a true value turns it off, and
    /// unset (or not a boolean) leaves `uploadpack.lazyFetch` in charge.
    ///
    /// Git's upload-pack sets `GIT_NO_LAZY_FETCH=1` unless the variable is already
    /// in its environment, so `GIT_NO_LAZY_FETCH=0` is how a Git server is told to
    /// fetch promised objects for clients that did not accept its promisor remotes.
    ///
    /// # Parameters
    ///
    /// - `env`: the server process environment.
    #[must_use]
    pub fn lazy_fetch_from_environment(env: &crate::environment::Environment) -> Option<bool> {
        [
            crate::promisor_fetch::NO_LAZY_FETCH_ENV,
            "GIT_NO_LAZY_FETCH",
        ]
        .into_iter()
        .find_map(|key| env.var(key))
        .and_then(|value| crate::config::parse_bool(value.trim()).ok())
        .map(|no_lazy_fetch| !no_lazy_fetch)
    }

    /// Whether upload-pack may fetch missing promised objects for this session:
    /// [`Self::lazy_fetch`] when set, otherwise `uploadpack.lazyFetch` from `config`.
    ///
    /// A caller uses this to decide whether to open the served [`Repository`] with a
    /// [`PromisorFetcher`](crate::promisor_fetch::PromisorFetcher). Without one, a
    /// served repository never fetches anything.
    ///
    /// # Parameters
    ///
    /// - `config`: the served repository's configuration.
    #[must_use]
    pub fn lazy_fetch_enabled(&self, config: &crate::config::ConfigSet) -> bool {
        self.lazy_fetch.unwrap_or_else(|| {
            config
                .get_bool("uploadpack.lazyfetch")
                .and_then(|v| v.ok())
                .unwrap_or(false)
        })
    }

    /// Give the repository an upload-pack session serves the promisor fetcher it may
    /// use: `fetcher` when [`Self::lazy_fetch_enabled`], and none otherwise, so a
    /// server without lazy fetch never fetches anything, whichever fetcher the
    /// repository was opened with.
    ///
    /// Every upload-pack entry point (the `grit upload-pack` command, smart HTTP,
    /// the in-process local transport) calls this right after opening the
    /// repository.
    ///
    /// # Parameters
    ///
    /// - `repo`: the served repository.
    /// - `config`: its configuration.
    /// - `fetcher`: the fetcher to use when lazy fetch is on.
    ///
    /// # Returns
    ///
    /// Whether a fetcher is now installed.
    pub fn attach_promisor_fetcher(
        &self,
        repo: &Repository,
        config: &crate::config::ConfigSet,
        fetcher: Option<std::sync::Arc<dyn crate::promisor_fetch::PromisorFetcher>>,
    ) -> bool {
        let fetcher = fetcher.filter(|_| self.lazy_fetch_enabled(config));
        let attached = fetcher.is_some();
        repo.caches().set_promisor_fetcher(fetcher);
        attached
    }
}

/// Name of the receive-pack capability that advertises a resumable push
/// endpoint ([`ServeOptions::push_bundle`]).
pub const PUSH_BUNDLE_CAPABILITY: &str = "pushBundle";

/// One ref as it appears in an advertisement.
#[derive(Debug, Clone)]
pub(crate) struct AdvertisedRef {
    /// Full ref name, such as `refs/heads/main`.
    pub(crate) name: String,
    /// Object the ref points at.
    pub(crate) oid: ObjectId,
    /// For an annotated tag, the non-tag object it ultimately points at.
    pub(crate) peeled: Option<ObjectId>,
}

/// Whether [`advertised_refs`] fills [`AdvertisedRef::peeled`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Peel {
    /// Leave `peeled` empty and read no objects.
    No,
    /// Peel annotated tags, trusting `packed-refs` peel lines where they apply.
    Yes,
}

/// List the repository's refs in advertisement order, skipping hidden ones.
///
/// Only refs whose names start with one of `prefixes` are listed (every ref
/// under `refs/` when `prefixes` is empty), and the ref store is asked for
/// those prefixes alone, so the cost follows the refs returned rather than the
/// refs stored. Prefixes that cannot name a ref under `refs/` (such as `HEAD`)
/// select nothing here.
///
/// Refs whose names are not safe to send and symbolic refs that do not resolve
/// are left out. As in Git, a ref naming a missing object is still advertised;
/// with [`Peel::Yes`] it, like any ref whose tag chain cannot be read, simply
/// gets no peeled value.
pub(crate) fn advertised_refs(
    repo: &Repository,
    hidden: &[String],
    prefixes: &[&str],
    peel: Peel,
) -> Result<Vec<AdvertisedRef>> {
    let store = repo.refs();
    let git_dir = repo.git_dir();
    let linked_worktree = crate::worktree_ref::is_linked_worktree_git_dir(git_dir);
    let mut listed: Vec<(String, ObjectId)> = Vec::new();
    for prefix in scan_prefixes(prefixes) {
        store.for_each_ref(prefix, &mut |entry| {
            let name = entry.name.as_str();
            if !name.starts_with(prefix)
                || !crate::refs::is_valid_fetch_advertised_ref(name)
                || crate::hide_refs::ref_is_hidden(name, name, hidden)
                || (linked_worktree
                    && !crate::worktree_ref::ref_visible_from_worktree(git_dir, name))
            {
                return ControlFlow::Continue(());
            }
            let oid = match &entry.value {
                RawRef::Direct(oid) => Some(*oid),
                RawRef::Symbolic(_) => entry.peeled.or_else(|| store.resolve(name).ok()),
            };
            if let Some(oid) = oid {
                listed.push((entry.name.clone(), oid));
            }
            ControlFlow::Continue(())
        })?;
    }
    if !listed.is_sorted_by(|a, b| a.0 < b.0) {
        listed.sort_by(|a, b| a.0.cmp(&b.0));
        listed.dedup_by(|later, first| later.0 == first.0);
    }

    if peel == Peel::No {
        return Ok(listed
            .into_iter()
            .map(|(name, oid)| AdvertisedRef {
                name,
                oid,
                peeled: None,
            })
            .collect());
    }
    let odb = repo.odb();
    // A malformed `packed-refs` only loses the shortcut; peeling falls back to the objects.
    let packed = (listed.len() >= PACKED_PEELS_MIN_REFS)
        .then(|| PackedRefPeels::for_store(store, git_dir).ok())
        .flatten();
    Ok(odb.with_pack_read_context(|| {
        listed
            .into_iter()
            .map(|(name, oid)| {
                let peeled = packed
                    .as_ref()
                    .and_then(|p| p.peeled(&name, &oid))
                    .unwrap_or_else(|| odb.peel_tag_target(oid).ok().flatten());
                AdvertisedRef { name, oid, peeled }
            })
            .collect()
    }))
}

/// Ref-store prefixes to list so that every ref under `refs/` starting with one
/// of `requested` is visited exactly once.
///
/// No request means all of `refs/`. A request shorter than `refs/` that it
/// begins (`""`, `"re"`) widens to `refs/`; one outside it selects nothing.
/// Prefixes covered by a shorter one are dropped, which leaves the result
/// sorted and disjoint: listing each in turn yields names in sorted order.
fn scan_prefixes<'a>(requested: &[&'a str]) -> Vec<&'a str> {
    const ALL: &str = "refs/";
    if requested.is_empty() {
        return vec![ALL];
    }
    let mut scan: Vec<&str> = requested
        .iter()
        .filter_map(|p| {
            if p.starts_with(ALL) {
                Some(*p)
            } else {
                ALL.starts_with(p).then_some(ALL)
            }
        })
        .collect();
    scan.sort_unstable();
    scan.dedup_by(|later, kept| later.starts_with(*kept));
    scan
}

/// Where `HEAD` points: its symbolic target (if any) and resolved object (if born).
pub(crate) struct HeadInfo {
    /// Ref `HEAD` names, such as `refs/heads/main`, when `HEAD` is symbolic.
    pub(crate) target: Option<String>,
    /// Object `HEAD` resolves to; `None` on an unborn branch.
    pub(crate) oid: Option<ObjectId>,
}

/// Read `HEAD` for the advertisement.
pub(crate) fn head_info(repo: &Repository) -> HeadInfo {
    HeadInfo {
        target: crate::refs::read_symbolic_ref(repo.git_dir(), "HEAD")
            .ok()
            .flatten(),
        oid: crate::refs::resolve_ref(repo.git_dir(), "HEAD").ok(),
    }
}

/// Write a pkt-line carrying `data` followed by a newline.
pub(crate) fn write_line(out: &mut dyn std::io::Write, data: &str) -> Result<()> {
    let mut buf = Vec::with_capacity(data.len() + 5);
    crate::pkt_line::write_line_to_vec(&mut buf, data)?;
    out.write_all(&buf)?;
    Ok(())
}

/// Write a flush packet.
pub(crate) fn write_flush(out: &mut dyn std::io::Write) -> Result<()> {
    out.write_all(crate::pkt_line::FLUSH.as_bytes())?;
    Ok(())
}

/// Read one packet, normalising a data line by dropping its trailing newline.
pub(crate) fn read_packet(input: &mut dyn std::io::Read) -> Result<Option<Packet>> {
    let pkt = crate::pkt_line::read_packet(&mut &mut *input)?;
    Ok(pkt.map(|p| match p {
        crate::pkt_line::Packet::Data(s) => Packet::Data(s.trim_end_matches('\n').to_owned()),
        crate::pkt_line::Packet::Flush => Packet::Flush,
        crate::pkt_line::Packet::Delim => Packet::Delim,
        crate::pkt_line::Packet::ResponseEnd => Packet::ResponseEnd,
    }))
}

/// A packet read from the client, with data lines already trimmed.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Packet {
    /// A data line without its trailing newline.
    Data(String),
    /// `0000`.
    Flush,
    /// `0001`.
    Delim,
    /// `0002`.
    ResponseEnd,
}

/// Parse a full-width hex object id for the repository's hash algorithm.
pub(crate) fn parse_oid(repo: &Repository, hex: &str) -> Result<ObjectId> {
    let oid = ObjectId::from_hex(hex)
        .map_err(|_| ServeError::Protocol(format!("invalid object id '{hex}'")))?;
    if oid.algo() != repo.odb().hash_algo() {
        return Err(ServeError::Protocol(format!(
            "object id '{hex}' does not match the repository hash algorithm"
        )));
    }
    Ok(oid)
}

/// Check a client's `object-format=` capability against the repository.
pub(crate) fn check_object_format(repo: &Repository, client: Option<&str>) -> Result<()> {
    let ours = repo.odb().hash_algo().name();
    match client {
        Some(name) if name != ours => Err(ServeError::ObjectFormatMismatch {
            client: name.to_owned(),
            repository: ours,
        }),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::{advertised_refs, scan_prefixes, Peel, ProtocolVersion};
    use crate::config::environment::RepositoryOptions;
    use crate::objects::{ObjectId, ObjectKind};
    use crate::repo::{InitOptions, Repository};

    #[test]
    fn scan_prefixes_collapse_to_disjoint_sorted_ref_prefixes() {
        assert_eq!(scan_prefixes(&[]), vec!["refs/"]);
        assert_eq!(scan_prefixes(&["HEAD"]), Vec::<&str>::new());
        assert_eq!(scan_prefixes(&["", "HEAD"]), vec!["refs/"]);
        assert_eq!(scan_prefixes(&["re", "refs/tags/"]), vec!["refs/"]);
        assert_eq!(
            scan_prefixes(&[
                "refs/tags/v1",
                "refs/heads/",
                "refs/tags/",
                "refs/heads/main"
            ]),
            vec!["refs/heads/", "refs/tags/"]
        );
        assert_eq!(
            scan_prefixes(&["refs/tags/v2", "refs/tags/v1", "refs/tags/v1"]),
            vec!["refs/tags/v1", "refs/tags/v2"]
        );
    }

    /// A bare repository with `main`, `feature`, an annotated tag `v1`, a hidden ref, and a
    /// ref naming a missing object.
    fn fixture() -> (tempfile::TempDir, Repository, ObjectId, ObjectId) {
        let dir = tempfile::tempdir().expect("tempdir");
        Repository::init(
            dir.path(),
            &InitOptions::new()
                .with_bare(true)
                .with_initial_branch("main"),
        )
        .expect("init");
        let repo = Repository::open(dir.path(), &RepositoryOptions::empty()).expect("open");
        let tree = repo.odb().write(ObjectKind::Tree, b"").expect("tree");
        let commit = repo
            .odb()
            .write(
                ObjectKind::Commit,
                format!("tree {tree}\nauthor A <a@e> 1 +0000\ncommitter A <a@e> 1 +0000\n\nc\n")
                    .as_bytes(),
            )
            .expect("commit");
        let tag = repo
            .odb()
            .write(
                ObjectKind::Tag,
                format!("object {commit}\ntype commit\ntag v1\ntagger A <a@e> 1 +0000\n\nv1\n")
                    .as_bytes(),
            )
            .expect("tag");
        let missing = ObjectId::from_hex("0123456789abcdef0123456789abcdef01234567").expect("oid");
        for (name, oid) in [
            ("refs/heads/main", commit),
            ("refs/heads/feature", commit),
            ("refs/heads/broken", missing),
            ("refs/hidden/x", commit),
            ("refs/tags/v1", tag),
        ] {
            let path = dir.path().join(name);
            std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
            std::fs::write(path, format!("{oid}\n")).expect("ref");
        }
        (dir, repo, commit, tag)
    }

    fn listed(refs: &[super::AdvertisedRef]) -> Vec<(&str, Option<ObjectId>)> {
        refs.iter().map(|r| (r.name.as_str(), r.peeled)).collect()
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn advertised_refs_lists_requested_prefixes_and_skips_hidden() {
        let (_dir, repo, commit, _tag) = fixture();
        let hidden = vec!["refs/hidden".to_owned()];

        let all = advertised_refs(&repo, &hidden, &[], Peel::Yes).expect("all");
        assert_eq!(
            listed(&all),
            vec![
                ("refs/heads/broken", None),
                ("refs/heads/feature", None),
                ("refs/heads/main", None),
                ("refs/tags/v1", Some(commit)),
            ]
        );

        let some = advertised_refs(
            &repo,
            &hidden,
            &["refs/tags/", "refs/heads/m", "HEAD", "refs/hidden/"],
            Peel::No,
        )
        .expect("prefixed");
        assert_eq!(
            listed(&some),
            vec![("refs/heads/main", None), ("refs/tags/v1", None)]
        );

        let none = advertised_refs(&repo, &hidden, &["HEAD"], Peel::Yes).expect("HEAD only");
        assert!(none.is_empty());
    }

    #[test]
    fn git_protocol_picks_highest_version() {
        assert_eq!(ProtocolVersion::from_git_protocol(""), ProtocolVersion::V0);
        assert_eq!(
            ProtocolVersion::from_git_protocol("version=1"),
            ProtocolVersion::V1
        );
        assert_eq!(
            ProtocolVersion::from_git_protocol("version=2:object-format=sha1"),
            ProtocolVersion::V2
        );
        assert_eq!(
            ProtocolVersion::from_git_protocol("version=9"),
            ProtocolVersion::V0
        );
    }
}
