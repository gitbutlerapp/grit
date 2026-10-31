//! Unified remote URL typing and transport dispatch for fetch, push, and ref listing.
//!
//! [`RemoteUrl`] classifies configured or literal remote URLs. [`Remote`] holds the
//! resolved URL(s), fetch refspecs, and dispatches to the appropriate grit-lib
//! transport (local, `git://`, SSH, smart HTTP).

use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use thiserror::Error;

use crate::config::ConfigSet;
#[cfg(feature = "http-ureq")]
use crate::credentials::{
    askpass_program, Credential, CredentialProvider, HelperCredentialProvider,
};
#[cfg(feature = "http-ureq")]
use crate::environment::Environment;
use crate::error::Error;
use crate::fetch::{fetch_remote_into, Progress};
use crate::objects::ObjectId;
use crate::odb::Odb;
use crate::push::push_remote_from;
use crate::refs::listing::{
    compare_version_names, patterns_match_ls_remote, RefQueryOptions, RefSort, SortKey,
};
use crate::repo::Repository;
use crate::transfer::{
    fetch_local_into, push_local_from, FetchOptions, FetchOutcome, PrePushGate, PushOptions,
    PushOutcome, PushRefSpec,
};
use crate::transport::http::{HttpClient, SmartHttpTransport};
use crate::transport::{ConnectOptions, GitDaemonTransport, Service, SshTransport, Transport};
use crate::transport_path::{
    is_local_path_remote_url, resolve_local_remote_git_dir, url_is_local_not_ssh,
};
use crate::url_rewrite;

#[cfg(feature = "http-ureq")]
use crate::transport::http::ureq_client::UreqHttpClient;

/// HTTP client types for non-git HTTP (e.g. OAuth) without pulling transport internals into binaries.
#[cfg(feature = "http-ureq")]
pub mod http_client {
    pub use crate::transport::http::ureq_client::UreqHttpClient;
    pub use crate::transport::http::HttpClient;
}

/// Result of a remote operation ([`RemoteError`] on failure).
pub type RemoteResult<T> = std::result::Result<T, RemoteError>;

/// Default remote name when none is configured (`origin`, matching Git).
pub const DEFAULT_REMOTE: &str = "origin";

/// Whether `name` can name a remote: its tracking refs `refs/remotes/<name>/…` must be valid.
#[must_use]
pub(crate) fn is_valid_remote_name(name: &str) -> bool {
    use crate::check_ref_format::{check_refname_format, RefNameOptions};
    !name.is_empty()
        && check_refname_format(
            &format!("refs/remotes/{name}/test"),
            &RefNameOptions::default(),
        )
        .is_ok()
}

/// Errors specific to remote URL resolution and dispatch.
#[derive(Debug, Error)]
pub enum RemoteError {
    /// A remote has no URL: `remote.<name>.url` is unset or blank, or a literal URL was empty.
    #[error("{}", match remote {
        Some(name) => format!("remote '{name}' has no URL configured"),
        None => "remote URL is empty".to_owned(),
    })]
    NoUrl {
        /// The configured remote's name; `None` when an empty literal URL was given.
        remote: Option<String>,
    },
    /// The URL uses a scheme grit does not support for remotes.
    #[error("unsupported remote URL scheme")]
    UnsupportedScheme,
    /// A URL or path failed to parse.
    #[error("{0}")]
    InvalidUrl(String),
    /// Smart HTTP requires the `http-ureq` feature or an injected HTTP client factory.
    #[error("HTTP remotes require the http-ureq feature or an HttpClientFactory")]
    HttpUnavailable,
    /// The argument is neither a configured remote name nor a URL or path.
    #[error("unknown remote '{0}'")]
    UnknownRemote(String),
    /// [`RepositoryRemotes::add`](crate::repo_handles::RepositoryRemotes::add) found an
    /// existing `remote.<name>.*` section.
    #[error("remote '{0}' already exists")]
    RemoteExists(String),
    /// The name cannot be used for a remote (`refs/remotes/<name>/…` would not be a valid ref).
    #[error("'{0}' is not a valid remote name")]
    InvalidRemoteName(String),
    /// An underlying library error.
    #[error(transparent)]
    Library(#[from] Error),
}

impl RemoteError {
    /// Whether the remote could not be read or written at all ([`Error::RemoteUnreadable`]).
    #[must_use]
    pub fn is_remote_unreadable(&self) -> bool {
        matches!(self, Self::Library(e) if e.is_remote_unreadable())
    }

    /// Whether the operation was cancelled ([`Error::Cancelled`]).
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        matches!(self, Self::Library(e) if e.is_cancelled())
    }
}

impl From<RemoteError> for Error {
    fn from(e: RemoteError) -> Self {
        match e {
            RemoteError::Library(err) => err,
            other => Error::Message(other.to_string()),
        }
    }
}

/// A parsed remote URL by transport kind.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RemoteUrl {
    /// Scheme-less or relative filesystem path (not `file://`).
    Local(PathBuf),
    /// `file://` URL decoded to a filesystem path.
    File(PathBuf),
    /// `git://` daemon URL (stored verbatim for the transport).
    Git(String),
    /// `ssh://`, `git+ssh://`, or scp-style SSH URL.
    Ssh(String),
    /// `http://` smart HTTP URL.
    Http(String),
    /// `https://` smart HTTP URL.
    Https(String),
    /// On-disk git bundle file (detected by signature, not extension).
    Bundle(PathBuf),
}

impl RemoteUrl {
    /// Whether this URL is fetched/pushed via a local filesystem path.
    #[must_use]
    pub fn is_local(&self) -> bool {
        matches!(self, Self::Local(_) | Self::File(_))
    }

    /// Wire URL string for transports that take a URL (not local paths).
    #[must_use]
    pub fn as_wire_url(&self) -> Option<&str> {
        match self {
            Self::Git(s) | Self::Ssh(s) | Self::Http(s) | Self::Https(s) => Some(s.as_str()),
            Self::Local(_) | Self::File(_) | Self::Bundle(_) => None,
        }
    }

    /// Original configured string form (paths normalized for display).
    #[must_use]
    pub fn to_url_string(&self) -> String {
        match self {
            Self::Local(p) | Self::File(p) | Self::Bundle(p) => p.to_string_lossy().into_owned(),
            Self::Git(s) | Self::Ssh(s) | Self::Http(s) | Self::Https(s) => s.clone(),
        }
    }
}

impl TryFrom<&str> for RemoteUrl {
    type Error = RemoteError;

    fn try_from(url: &str) -> std::result::Result<Self, Self::Error> {
        parse_remote_url(url)
    }
}

/// Parse `url` into a [`RemoteUrl`] without applying config rewrites.
pub fn parse_remote_url(url: &str) -> RemoteResult<RemoteUrl> {
    let url = url.trim();
    if url.is_empty() {
        return Err(RemoteError::NoUrl { remote: None });
    }
    if url.starts_with("file://") {
        let path = crate::transport_path::file_url_to_local_path(url)
            .map_err(|e| RemoteError::InvalidUrl(e.to_string()))?;
        let path = PathBuf::from(path);
        if crate::bundle_remote::is_bundle_path(&path) {
            return Ok(RemoteUrl::Bundle(path));
        }
        return Ok(RemoteUrl::File(path));
    }
    if url.starts_with("git://") {
        crate::transport::parse_git_url(url).map_err(|e| RemoteError::InvalidUrl(e.to_string()))?;
        return Ok(RemoteUrl::Git(url.to_owned()));
    }
    if url.starts_with("http://") {
        return Ok(RemoteUrl::Http(url.to_owned()));
    }
    if url.starts_with("https://") {
        return Ok(RemoteUrl::Https(url.to_owned()));
    }
    if crate::transport::is_ssh_url(url) {
        crate::transport::parse_ssh_url(url).map_err(|e| RemoteError::InvalidUrl(e.to_string()))?;
        return Ok(RemoteUrl::Ssh(url.to_owned()));
    }
    if is_local_path_remote_url(url) && url_is_local_not_ssh(url) {
        let path = PathBuf::from(url);
        if crate::bundle_remote::is_bundle_path(&path) {
            return Ok(RemoteUrl::Bundle(path));
        }
        return Ok(RemoteUrl::Local(path));
    }
    Err(RemoteError::UnsupportedScheme)
}

/// A single reference returned by [`Remote::list_refs`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteRef {
    /// Full reference name (`HEAD`, `refs/heads/main`, `refs/tags/v1^{}`, …).
    pub name: String,
    /// Object id the ref resolves to.
    pub oid: ObjectId,
    /// Symbolic-ref target when [`ListRefsOptions::symrefs`] is set (`HEAD` only on v0/v1).
    pub symref_target: Option<String>,
}

impl RemoteRef {
    /// For a peeled `refs/tags/<tag>^{}` line ([`ListRefsOptions::peel`]), the tag ref it
    /// belongs to; `None` for every other ref.
    #[must_use]
    pub fn peeled_tag(&self) -> Option<&str> {
        self.name.strip_suffix("^{}")
    }
}

/// One advertised ref with its peeled `^{}` line folded in (see [`fold_peeled_refs`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeeledRemoteRef {
    /// Full reference name (never ends in `^{}`).
    pub name: String,
    /// Object id the ref points at (the tag object for annotated tags).
    pub oid: ObjectId,
    /// Object the annotated tag peels to, when the listing carried a `^{}` line for it.
    pub peeled: Option<ObjectId>,
    /// Symbolic-ref target, as in [`RemoteRef::symref_target`].
    pub symref_target: Option<String>,
}

/// Fold peeled `^{}` lines from a [`Remote::list_refs`] result into their tag refs.
///
/// Keeps the order of `refs` and drops the `^{}` rows themselves.
#[must_use]
pub fn fold_peeled_refs(refs: &[RemoteRef]) -> Vec<PeeledRemoteRef> {
    let peeled: std::collections::HashMap<&str, ObjectId> = refs
        .iter()
        .filter_map(|r| Some((r.peeled_tag()?, r.oid)))
        .collect();
    refs.iter()
        .filter(|r| r.peeled_tag().is_none())
        .map(|r| PeeledRemoteRef {
            name: r.name.clone(),
            oid: r.oid,
            peeled: peeled.get(r.name.as_str()).copied(),
            symref_target: r.symref_target.clone(),
        })
        .collect()
}

/// Options controlling [`Remote::list_refs`] output, aligned with `git ls-remote`.
#[derive(Debug, Default, Clone)]
pub struct ListRefsOptions {
    /// If non-empty, only refs matching one of these patterns (same rules as `git ls-remote`).
    pub prefixes: Vec<String>,
    /// Restrict to `refs/heads/` entries.
    pub heads: bool,
    /// Restrict to `refs/tags/` entries.
    pub tags: bool,
    /// Include symbolic-ref targets on `HEAD` (and all symrefs on v2 when supported).
    pub symrefs: bool,
    /// Append peeled `^{}` lines for annotated tags.
    pub peel: bool,
    /// Requested wire protocol version for remote listing (`None` → prefer v2).
    pub protocol_version: Option<u8>,
    /// Sort key and direction (after filtering; matches `git ls-remote --sort`).
    pub sort: RefSort,
    /// Maximum number of lines to return after sorting (`None` = unlimited).
    pub limit: Option<usize>,
    /// `versionsort.suffix` values for [`SortKey::Version`] (Git multi-valued config).
    pub versionsort_suffixes: Vec<String>,
}

/// Builds HTTP clients for smart-HTTP remotes.
pub trait HttpClientFactory: Send + Sync {
    /// Create a client honoring repository HTTP/credential config.
    ///
    /// # Errors
    ///
    /// Returns an error when the client cannot be constructed.
    fn create(&self, config: &ConfigSet) -> RemoteResult<Box<dyn HttpClient>>;

    /// Create a client for a best-effort request whose failure the caller ignores,
    /// such as the advertisement [`load_push_plan_context`](crate::remote::push_plan::load_push_plan_context)
    /// reads before a push.
    ///
    /// Such a client must not erase credentials the server refuses (no
    /// [`CredentialProvider::reject`](crate::credentials::CredentialProvider::reject)):
    /// the request that reports the failure does that, so it can tell the user the
    /// stored credentials were rejected rather than finding none. A factory whose
    /// clients never reject (or carry no credentials) can return
    /// [`create`](Self::create)'s client.
    ///
    /// # Errors
    ///
    /// Returns an error when the client cannot be constructed.
    fn create_for_probe(&self, config: &ConfigSet) -> RemoteResult<Box<dyn HttpClient>>;
}

/// An [`HttpClientFactory`] whose clients are all [probes](HttpClientFactory::create_for_probe)
/// of the factory a request would otherwise use (the caller's, or the default one).
pub(crate) struct ProbeHttpClientFactory<'a> {
    inner: Option<&'a dyn HttpClientFactory>,
    #[cfg(feature = "http-ureq")]
    default: DefaultHttpClientFactory,
}

impl<'a> ProbeHttpClientFactory<'a> {
    /// Probes of `inner`, or of the default factory for `repo` when there is none.
    pub(crate) fn new(inner: Option<&'a dyn HttpClientFactory>, repo: &Repository) -> Self {
        #[cfg(not(feature = "http-ureq"))]
        let _ = repo;
        Self {
            inner,
            #[cfg(feature = "http-ureq")]
            default: DefaultHttpClientFactory::with_environment(repo.environment().clone()),
        }
    }
}

impl HttpClientFactory for ProbeHttpClientFactory<'_> {
    fn create(&self, config: &ConfigSet) -> RemoteResult<Box<dyn HttpClient>> {
        if let Some(inner) = self.inner {
            return inner.create_for_probe(config);
        }
        #[cfg(feature = "http-ureq")]
        {
            self.default.create_for_probe(config)
        }
        #[cfg(not(feature = "http-ureq"))]
        {
            let _ = config;
            Err(RemoteError::HttpUnavailable)
        }
    }

    fn create_for_probe(&self, config: &ConfigSet) -> RemoteResult<Box<dyn HttpClient>> {
        self.create(config)
    }
}

/// Fills and approves through the wrapped provider, but never rejects.
#[cfg(feature = "http-ureq")]
struct KeepRejectedCredentials<P>(P);

#[cfg(feature = "http-ureq")]
impl<P: CredentialProvider> CredentialProvider for KeepRejectedCredentials<P> {
    fn fill(&self, input: &Credential) -> crate::error::Result<Credential> {
        self.0.fill(input)
    }

    fn approve(&self, cred: &Credential) -> crate::error::Result<()> {
        self.0.approve(cred)
    }

    fn reject(&self, _cred: &Credential) -> crate::error::Result<()> {
        Ok(())
    }
}

/// Default [`HttpClientFactory`] using `UreqHttpClient` (`http-ureq` feature).
///
/// Clients authenticate through the configured credential helpers, then the
/// askpass program from [`askpass_program`]. [`Default`] consults
/// `core.askPass` only; build with
/// [`with_environment`](DefaultHttpClientFactory::with_environment) to also
/// honor `GIT_ASKPASS` and `SSH_ASKPASS`.
#[cfg(feature = "http-ureq")]
#[derive(Debug, Default, Clone)]
pub struct DefaultHttpClientFactory {
    environment: Option<Environment>,
}

#[cfg(feature = "http-ureq")]
impl DefaultHttpClientFactory {
    /// A factory that consults config only (same as [`Default`]); usable in a
    /// `static`.
    #[must_use]
    pub const fn new() -> Self {
        Self { environment: None }
    }

    /// A factory that reads `GIT_ASKPASS` / `SSH_ASKPASS` (and `HOME` for a
    /// `~/` in `core.askPass`) from `environment`.
    #[must_use]
    pub fn with_environment(environment: Environment) -> Self {
        Self {
            environment: Some(environment),
        }
    }

    fn credential_provider(&self, config: &ConfigSet) -> HelperCredentialProvider {
        let askpass = match &self.environment {
            Some(env) => askpass_program(env, config),
            None => askpass_program(&Environment::empty(), config),
        };
        HelperCredentialProvider::new(config.clone()).with_askpass(askpass)
    }

    fn client(
        config: &ConfigSet,
        provider: Box<dyn CredentialProvider + Send + Sync>,
    ) -> RemoteResult<Box<dyn HttpClient>> {
        let client = UreqHttpClient::from_config(config)
            .map_err(|e| RemoteError::InvalidUrl(e.to_string()))?
            .with_credential_provider(provider);
        Ok(Box::new(client))
    }
}

#[cfg(feature = "http-ureq")]
impl HttpClientFactory for DefaultHttpClientFactory {
    fn create(&self, config: &ConfigSet) -> RemoteResult<Box<dyn HttpClient>> {
        Self::client(config, Box::new(self.credential_provider(config)))
    }

    fn create_for_probe(&self, config: &ConfigSet) -> RemoteResult<Box<dyn HttpClient>> {
        Self::client(
            config,
            Box::new(KeepRejectedCredentials(self.credential_provider(config))),
        )
    }
}

/// A resolved remote: name (if from config), URL(s), and fetch refspecs.
#[derive(Clone, Debug)]
pub struct Remote {
    /// Configured remote name, if any.
    pub name: Option<String>,
    fetch_url: RemoteUrl,
    push_url: Option<RemoteUrl>,
    /// Fetch refspecs (`+refs/heads/*:refs/remotes/<name>/*` when unset in config).
    pub fetch_refspecs: Vec<String>,
}

impl Remote {
    /// Load `remote.<name>.url`, optional `pushurl`, and `fetch` refspecs from `config`.
    ///
    /// Applies `url.*.insteadOf` / `pushInsteadOf` rewrites.
    ///
    /// # Errors
    ///
    /// Returns [`RemoteError::NoUrl`] when the remote has no URL.
    pub fn from_config(config: &ConfigSet, name: &str) -> RemoteResult<Self> {
        let raw_url = config
            .get(&format!("remote.{name}.url"))
            .filter(|u| !u.trim().is_empty())
            .ok_or_else(|| RemoteError::NoUrl {
                remote: Some(name.to_owned()),
            })?;
        let fetch_url = url_rewrite::rewrite_fetch_url(config, &raw_url);
        let push_source = config
            .get(&format!("remote.{name}.pushurl"))
            .filter(|u| !u.trim().is_empty())
            .unwrap_or_else(|| raw_url.clone());
        let push_url = url_rewrite::rewrite_push_url(config, push_source.as_str());
        let mut fetch_refspecs = config.get_all(&format!("remote.{name}.fetch"));
        fetch_refspecs.retain(|s| !s.trim().is_empty());
        if fetch_refspecs.is_empty() {
            fetch_refspecs = vec![format!("+refs/heads/*:refs/remotes/{name}/*")];
        }
        Ok(Self {
            name: Some(name.to_owned()),
            fetch_url: RemoteUrl::try_from(fetch_url.as_str())?,
            push_url: Some(RemoteUrl::try_from(push_url.as_str())?),
            fetch_refspecs,
        })
    }

    /// Default refspec for [`Self::from_url`] — match Git's anonymous fetch: download
    /// advertised heads but do not write remote-tracking refs (only [`FETCH_HEAD`](https://git-scm.com/docs/git-fetch#_discussion)).
    pub const ANONYMOUS_FETCH_REFSPEC: &'static str = "refs/heads/*:";

    /// Build a remote from a literal URL or path (not a configured remote name).
    ///
    /// Uses [`Self::ANONYMOUS_FETCH_REFSPEC`]; callers that need tracking refs must
    /// pass explicit [`FetchOptions::refspecs`].
    ///
    /// # Errors
    ///
    /// Returns an error when `url` is empty or unsupported.
    pub fn from_url(url: &str) -> RemoteResult<Self> {
        Ok(Self {
            name: None,
            fetch_url: RemoteUrl::try_from(url)?,
            push_url: None,
            fetch_refspecs: vec![Self::ANONYMOUS_FETCH_REFSPEC.to_owned()],
        })
    }

    /// Fetch URL after config rewrite (for tests and display).
    #[must_use]
    pub fn fetch_url(&self) -> &RemoteUrl {
        &self.fetch_url
    }

    /// Push URL when `remote.*.pushurl` is set, else the fetch URL.
    #[must_use]
    pub fn push_url(&self) -> &RemoteUrl {
        self.push_url.as_ref().unwrap_or(&self.fetch_url)
    }

    /// Fetch from this remote into `repo`.
    ///
    /// `http_factory` is required for HTTP(S) remotes unless the `http-ureq` feature is
    /// enabled and `None` is passed (then the default ureq-backed factory is used).
    ///
    /// # Errors
    ///
    /// Returns [`RemoteError`] or transport errors on failure. Received pack bytes are written
    /// into the repository's [`OdbBuilder::primary`](crate::odb::OdbBuilder::primary) store
    /// (including custom backends) via [`ingest_received_pack`](crate::index_pack::ingest_received_pack).
    pub fn fetch(
        &self,
        repo: &Repository,
        mut opts: FetchOptions,
        progress: &mut dyn Progress,
        cancel: &dyn crate::progress::Cancel,
        http_factory: Option<&dyn HttpClientFactory>,
    ) -> RemoteResult<FetchOutcome> {
        if opts.configured_refspecs.is_empty() {
            opts.configured_refspecs = self.fetch_refspecs.clone();
        }
        if opts.refspecs.is_empty() {
            opts.refspecs = self.fetch_refspecs.clone();
        }
        if opts.remote_name.is_none() {
            opts.remote_name = self.name.clone();
        }
        if opts.fetch_source.is_none() {
            opts.fetch_source = Some(self.fetch_url.to_url_string());
        }
        if opts.diagnostics.is_none() {
            opts.diagnostics = Some(repo.diagnostics());
            opts.network_trace = repo.network_trace_enabled();
        }
        let protocol_version = upload_pack_protocol_version(opts.http_protocol_version, repo)?;
        if opts.http_protocol_version.is_none() {
            opts.http_protocol_version = Some(protocol_version);
        }
        let connect = ConnectOptions {
            protocol_version,
            diagnostics: opts.diagnostics.clone(),
            network_trace: opts.network_trace,
            ..Default::default()
        };
        self.dispatch_fetch(
            repo,
            &self.fetch_url,
            &mut opts,
            progress,
            cancel,
            http_factory,
            connect,
        )
    }

    /// Push `refs` to this remote from `repo`.
    ///
    /// `cancel` is polled throughout: while the objects to send are walked, while the
    /// pack is built and streamed, while a local (`file://` or path) remote indexes
    /// it, and while a smart-HTTP push waits for the server's report. A cancel
    /// before the objects are stored updates no remote ref and leaves no temporary
    /// files; a cancel after a wire push sent its whole request stops waiting, but
    /// the server may still apply the update. Remote-tracking refs are not updated
    /// either way.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Cancelled`] (see [`RemoteError::is_cancelled`]) once `cancel`
    /// fires, or [`RemoteError`] / transport errors on failure. The pack is built from
    /// [`Odb::read_store`](crate::odb::Odb::read_store), including custom primaries.
    pub fn push(
        &self,
        repo: &Repository,
        refs: &[PushRefSpec],
        mut opts: PushOptions,
        progress: &mut dyn Progress,
        cancel: &dyn crate::progress::Cancel,
        http_factory: Option<&dyn HttpClientFactory>,
    ) -> RemoteResult<PushOutcome> {
        if opts.tracking_remote.is_none() {
            opts.tracking_remote = self.name.clone();
        }
        if opts.resumable_threshold.is_none() {
            opts.resumable_threshold = repo
                .config()
                .ok()
                .and_then(|c| c.get_i64("push.bundleThreshold").and_then(|r| r.ok()))
                .and_then(|v| u64::try_from(v).ok());
        }
        let url = pre_push_hook_url(self.push_url());
        let gate = PrePushGate {
            repo,
            remote: self.name.as_deref().unwrap_or(&url),
            url: &url,
        };
        self.dispatch_push(
            repo,
            self.push_url(),
            refs,
            &opts,
            (!opts.no_verify).then_some(&gate),
            progress,
            cancel,
            http_factory,
            ConnectOptions {
                protocol_version: 0,
                diagnostics: Some(repo.diagnostics()),
                network_trace: repo.network_trace_enabled(),
                ..Default::default()
            },
        )
    }

    /// Seed the empty repository `repo` from the bundle this remote advertises
    /// with `bundle-uri` (see [`crate::bundle_uri::seed_from_bundle_uri`]).
    ///
    /// Only smart-HTTP remotes advertise bundles; other transports, and
    /// `transfer.bundleURI=false` in `repo`'s config, give
    /// [`BundleSeed::NotOffered`](crate::bundle_uri::BundleSeed::NotOffered).
    ///
    /// # Parameters
    ///
    /// - `repo`: the new repository the clone is filling.
    /// - `cache_dir`: where bundle downloads are kept between attempts.
    /// - `progress`: receives the download and unpack progress.
    /// - `cancel`: checked while the bundle is downloaded and unpacked; a
    ///   cancelled seed comes back as `Failed`, so callers check `cancel` again.
    /// - `http_factory`: builds the HTTP client, as for [`Self::fetch`]. Seeding is
    ///   best-effort, so it uses a [probe client](HttpClientFactory::create_for_probe):
    ///   credentials the server refuses stay stored for the fetch that follows to
    ///   report.
    #[must_use]
    pub fn seed_from_bundle_uri(
        &self,
        repo: &Repository,
        cache_dir: &Path,
        progress: &mut dyn Progress,
        cancel: &dyn crate::progress::Cancel,
        http_factory: Option<&dyn HttpClientFactory>,
    ) -> crate::bundle_uri::BundleSeed {
        use crate::bundle_uri::BundleSeed;
        let (RemoteUrl::Http(url) | RemoteUrl::Https(url)) = &self.fetch_url else {
            return BundleSeed::NotOffered;
        };
        let enabled = repo
            .config()
            .ok()
            .and_then(|c| c.get_bool("transfer.bundleURI").and_then(|r| r.ok()))
            .unwrap_or(true);
        if !enabled {
            return BundleSeed::NotOffered;
        }
        let probe = ProbeHttpClientFactory::new(http_factory, repo);
        let client = match http_client(Some(&probe), repo) {
            Ok(c) => c,
            Err(e) => {
                return BundleSeed::Failed {
                    uri: None,
                    reason: e.to_string(),
                }
            }
        };
        crate::bundle_uri::seed_from_bundle_uri(
            repo,
            client.as_ref(),
            url,
            cache_dir,
            progress,
            cancel,
        )
    }

    /// List references on this remote (all transports), matching `git ls-remote` ordering.
    ///
    /// `repo` supplies git-dir/worktree for relative local URLs and optional ODB for peeling.
    ///
    /// # Errors
    ///
    /// Returns [`RemoteError`] on resolution or wire failures.
    pub fn list_refs(
        &self,
        repo: Option<&Repository>,
        opts: &ListRefsOptions,
        http_factory: Option<&dyn HttpClientFactory>,
    ) -> RemoteResult<Vec<RemoteRef>> {
        let (git_dir, work_tree, odb) = repo_context(repo);
        match &self.fetch_url {
            RemoteUrl::Bundle(path) => list_refs_from_bundle(path, opts),
            RemoteUrl::Local(_) | RemoteUrl::File(_) => {
                let remote_git = local_git_dir_from_url(&self.fetch_url, git_dir, work_tree)?;
                list_refs_from_git_dir(&remote_git, None, opts)
            }
            wire => self.list_refs_wire(wire, git_dir, odb.as_ref(), opts, http_factory, repo),
        }
    }

    /// List the refs a push to this remote's [`push_url`](Self::push_url) starts from.
    ///
    /// Local and `file://` destinations are read with one scan of every ref under `refs/`, the
    /// set `git receive-pack` advertises (no `HEAD` line). Other transports return the
    /// [`list_refs`](Self::list_refs) listing of the push URL.
    ///
    /// # Errors
    ///
    /// Returns [`RemoteError`] on resolution or wire failures, and
    /// [`RemoteError::UnsupportedScheme`] for bundles.
    pub fn list_push_refs(
        &self,
        repo: &Repository,
        http_factory: Option<&dyn HttpClientFactory>,
    ) -> RemoteResult<Vec<RemoteRef>> {
        let url = self.push_url();
        match url {
            RemoteUrl::Bundle(_) => Err(RemoteError::UnsupportedScheme),
            RemoteUrl::Local(_) | RemoteUrl::File(_) => {
                let remote_git = local_git_dir_from_url(url, repo.git_dir(), repo.work_tree())?;
                let refs =
                    crate::refs::list_refs(&remote_git, "refs/").map_err(RemoteError::Library)?;
                Ok(refs
                    .into_iter()
                    .map(|(name, oid)| RemoteRef {
                        name,
                        oid,
                        symref_target: None,
                    })
                    .collect())
            }
            wire => self.list_refs_wire(
                wire,
                repo.git_dir(),
                Some(repo.odb()),
                &ListRefsOptions::default(),
                http_factory,
                Some(repo),
            ),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn dispatch_fetch(
        &self,
        repo: &Repository,
        url: &RemoteUrl,
        opts: &mut FetchOptions,
        progress: &mut dyn Progress,
        cancel: &dyn crate::progress::Cancel,
        http_factory: Option<&dyn HttpClientFactory>,
        connect: ConnectOptions,
    ) -> RemoteResult<FetchOutcome> {
        match url {
            RemoteUrl::Bundle(path) => Ok(crate::bundle_remote::fetch_from_bundle_into(
                repo, path, opts, progress, cancel,
            )?),
            RemoteUrl::Local(_) | RemoteUrl::File(_) => {
                let remote_git = local_git_dir_from_url(url, repo.git_dir(), repo.work_tree())?;
                let cfg = ConfigSet::load(&opts.environment, Some(repo.git_dir()), true)
                    .map_err(RemoteError::Library)?;
                if opts.filter.is_none() {
                    if let Some(name) = opts.remote_name.as_deref().or(self.name.as_deref()) {
                        opts.filter = crate::partial_clone::stored_remote_filter(&cfg, name);
                    }
                }
                // An automatic filter needs the server's promisor advertisement, which
                // only the wire protocol carries. A source that is itself backed by a
                // promisor remote (a server whose large blobs live on a LOP) lacks some
                // objects, so copying its object store would come up short: it is
                // served like a remote, which omits or fetches what it lacks.
                if opts.auto_filter
                    || crate::partial_clone::fetch_requires_upload_pack(
                        opts.filter.as_ref(),
                        &cfg,
                        opts.remote_name.as_deref().or(self.name.as_deref()),
                    )
                    || local_source_has_promisor_remotes(&remote_git)
                {
                    let client_env = if opts.environment.is_fetch_options_default() {
                        repo.environment()
                    } else {
                        &opts.environment
                    };
                    let mut conn = crate::transport::connect_local_upload_pack(
                        &remote_git,
                        Service::UploadPack,
                        &connect,
                        client_env,
                        repo.caches().promisor_fetcher(),
                    )
                    .map_err(RemoteError::Library)?;
                    let fetched = fetch_remote_into(repo, &mut *conn, opts, progress, cancel);
                    // A serving side that failed knows why (the client may only have
                    // seen the stream end early, or even taken it as an empty pack), so
                    // its error comes first.
                    conn.finish().map_err(RemoteError::Library)?;
                    return Ok(fetched?);
                }
                Ok(fetch_local_into(repo, &remote_git, opts, progress, cancel)?)
            }
            RemoteUrl::Http(u) | RemoteUrl::Https(u) => {
                let client: Arc<dyn HttpClient> = Arc::from(http_client(http_factory, repo)?);
                let transport = SmartHttpTransport::new(client);
                let mut conn = transport.connect(u, Service::UploadPack, &connect)?;
                Ok(fetch_remote_into(repo, &mut *conn, opts, progress, cancel)?)
            }
            RemoteUrl::Git(u) => {
                let mut conn =
                    GitDaemonTransport::new().connect(u, Service::UploadPack, &connect)?;
                Ok(fetch_remote_into(repo, &mut *conn, opts, progress, cancel)?)
            }
            RemoteUrl::Ssh(u) => {
                let transport = ssh_transport(Some(repo), Some(&opts.environment));
                let mut conn = transport.connect(u, Service::UploadPack, &connect)?;
                Ok(fetch_remote_into(repo, &mut *conn, opts, progress, cancel)?)
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn dispatch_push(
        &self,
        repo: &Repository,
        url: &RemoteUrl,
        refs: &[PushRefSpec],
        opts: &PushOptions,
        pre_push: Option<&PrePushGate<'_>>,
        progress: &mut dyn Progress,
        cancel: &dyn crate::progress::Cancel,
        http_factory: Option<&dyn HttpClientFactory>,
        connect: ConnectOptions,
    ) -> RemoteResult<PushOutcome> {
        let mut over_wire = |conn: &mut dyn crate::transport::Connection| {
            push_remote_from(repo, conn, refs, opts, progress, cancel, pre_push)
        };
        match url {
            RemoteUrl::Local(_) | RemoteUrl::File(_) => {
                let remote_git = local_git_dir_from_url(url, repo.git_dir(), repo.work_tree())?;
                Ok(push_local_from(
                    repo,
                    &remote_git,
                    refs,
                    opts,
                    progress,
                    cancel,
                    pre_push,
                )?)
            }
            RemoteUrl::Http(u) | RemoteUrl::Https(u) => {
                let client: Arc<dyn HttpClient> = Arc::from(http_client(http_factory, repo)?);
                let transport = SmartHttpTransport::new(client.clone());
                let mut conn = transport.connect(u, Service::ReceivePack, &connect)?;
                Ok(crate::push::push_remote_with(
                    repo,
                    &mut *conn,
                    refs,
                    opts,
                    progress,
                    cancel,
                    Some(crate::push::BundlePushTarget {
                        client: client.as_ref(),
                        repo_url: u,
                    }),
                    pre_push,
                )?)
            }
            RemoteUrl::Git(u) => {
                let mut conn =
                    GitDaemonTransport::new().connect(u, Service::ReceivePack, &connect)?;
                Ok(over_wire(&mut *conn)?)
            }
            RemoteUrl::Ssh(u) => {
                let transport = ssh_transport(Some(repo), None);
                let mut conn = transport.connect(u, Service::ReceivePack, &connect)?;
                Ok(over_wire(&mut *conn)?)
            }
            RemoteUrl::Bundle(_) => Err(RemoteError::UnsupportedScheme),
        }
    }

    fn list_refs_wire(
        &self,
        url: &RemoteUrl,
        local_git_dir: &Path,
        odb: Option<&Odb>,
        opts: &ListRefsOptions,
        http_factory: Option<&dyn HttpClientFactory>,
        repo: Option<&Repository>,
    ) -> RemoteResult<Vec<RemoteRef>> {
        let wire = url.as_wire_url().ok_or(RemoteError::UnsupportedScheme)?;
        let mut opts = opts.clone();
        if opts.protocol_version.is_none() {
            opts.protocol_version = Some(list_refs_protocol_version(None, repo)?);
        }
        let requested = opts.protocol_version.unwrap_or(2);
        let connect_opts = ConnectOptions {
            protocol_version: requested,
            ..Default::default()
        };
        match url {
            RemoteUrl::Http(_) | RemoteUrl::Https(_) => {
                list_refs_http(wire, local_git_dir, odb, &opts, http_factory, repo)
            }
            RemoteUrl::Git(_) => {
                let mut conn =
                    GitDaemonTransport::new().connect(wire, Service::UploadPack, &connect_opts)?;
                list_refs_from_connection(&mut *conn, local_git_dir, odb, &opts)
            }
            RemoteUrl::Ssh(_) => {
                let transport = ssh_transport(repo, None);
                let mut conn = transport.connect(wire, Service::UploadPack, &connect_opts)?;
                list_refs_from_connection(&mut *conn, local_git_dir, odb, &opts)
            }
            RemoteUrl::Local(_) | RemoteUrl::File(_) | RemoteUrl::Bundle(_) => unreachable!(),
        }
    }
}

/// The push URL as the `pre-push` hook sees it: `file://` URLs keep their scheme.
fn pre_push_hook_url(url: &RemoteUrl) -> String {
    match url {
        RemoteUrl::File(path) => format!("file://{}", path.to_string_lossy()),
        other => other.to_url_string(),
    }
}

fn repo_context(repo: Option<&Repository>) -> (&Path, Option<&Path>, Option<Odb>) {
    match repo {
        Some(r) => (r.git_dir(), r.work_tree(), Some(r.odb().clone())),
        None => (Path::new("."), None, None),
    }
}

fn upload_pack_protocol_version(explicit: Option<u8>, repo: &Repository) -> RemoteResult<u8> {
    if let Some(v) = explicit {
        return Ok(v);
    }
    crate::protocol::try_effective_client_protocol_version_from_config(
        repo.config().map_err(RemoteError::Library)?.as_ref(),
    )
    .map_err(RemoteError::Library)
}

fn list_refs_protocol_version(explicit: Option<u8>, repo: Option<&Repository>) -> RemoteResult<u8> {
    if let Some(v) = explicit {
        return Ok(v);
    }
    if let Some(repo) = repo {
        return upload_pack_protocol_version(None, repo);
    }
    Ok(2)
}

/// Whether the local repository at `git_dir` has promisor remotes of its own
/// (only its own config counts, as for any served repository's partial state).
fn local_source_has_promisor_remotes(git_dir: &Path) -> bool {
    ConfigSet::load_repo_local_only(git_dir)
        .is_ok_and(|cfg| crate::promisor::repo_treats_promisor_packs(git_dir, &cfg))
}

fn local_git_dir_from_url(
    url: &RemoteUrl,
    git_dir: &Path,
    work_tree: Option<&Path>,
) -> RemoteResult<PathBuf> {
    let s = url.to_url_string();
    let remote_git = resolve_local_remote_git_dir(&s, git_dir, work_tree);
    crate::transfer::ensure_local_remote_repository(&remote_git)?;
    Ok(remote_git)
}

fn ssh_transport(
    repo: Option<&Repository>,
    fetch_env: Option<&crate::environment::Environment>,
) -> SshTransport {
    let mut transport = SshTransport::new();
    if let Some(r) = repo {
        transport = transport.with_command_runner(r.command_runner());
    }
    let env = ssh_wire_environment(repo, fetch_env);
    transport = transport.with_ssh_environment(std::sync::Arc::new(env.clone()));
    if let Some(r) = repo {
        if let Ok(config) = r.config() {
            transport = transport.with_ssh_config(config);
        }
    } else if let Ok(config) = ConfigSet::load(&env, None, true) {
        transport = transport.with_ssh_config(Arc::new(config));
    }
    transport
}

/// Environment for SSH subprocess spawn: repository discovery env unless the fetch
/// options carry an explicit non-default [`FetchOptions::environment`] snapshot.
fn ssh_wire_environment(
    repo: Option<&Repository>,
    fetch_env: Option<&crate::environment::Environment>,
) -> crate::environment::Environment {
    if let Some(fe) = fetch_env {
        if !fe.is_fetch_options_default() {
            return fe.clone();
        }
    }
    repo.map(|r| r.environment().clone())
        .unwrap_or_else(crate::environment::Environment::capture_process)
}

fn http_client(
    factory: Option<&dyn HttpClientFactory>,
    repo: &Repository,
) -> RemoteResult<Box<dyn HttpClient>> {
    let config = repo.config().map_err(RemoteError::Library)?;
    if let Some(f) = factory {
        return f.create(config.as_ref());
    }
    #[cfg(feature = "http-ureq")]
    {
        DefaultHttpClientFactory::with_environment(repo.environment().clone())
            .create(config.as_ref())
    }
    #[cfg(not(feature = "http-ureq"))]
    {
        let _ = config;
        Err(RemoteError::HttpUnavailable)
    }
}

fn list_refs_http(
    repo_url: &str,
    local_git_dir: &Path,
    odb: Option<&Odb>,
    opts: &ListRefsOptions,
    http_factory: Option<&dyn HttpClientFactory>,
    repo: Option<&Repository>,
) -> RemoteResult<Vec<RemoteRef>> {
    let config: Arc<ConfigSet> = if let Some(r) = repo {
        r.config().map_err(RemoteError::Library)?
    } else {
        Arc::new(ConfigSet::new())
    };
    let client: Box<dyn HttpClient> = match http_factory {
        Some(f) => f.create(config.as_ref())?,
        None => {
            #[cfg(feature = "http-ureq")]
            {
                repo.map_or_else(DefaultHttpClientFactory::default, |r| {
                    DefaultHttpClientFactory::with_environment(r.environment().clone())
                })
                .create(config.as_ref())?
            }
            #[cfg(not(feature = "http-ureq"))]
            {
                return Err(RemoteError::HttpUnavailable);
            }
        }
    };

    // When `protocol_version` is set, honor it on discovery (including v0 with no
    // `Git-Protocol` header). `None` defers to the HTTP client's config default.
    let protocol_header_value = opts
        .protocol_version
        .and_then(crate::protocol::client_git_protocol_header_value);
    let git_protocol: Option<&str> = match opts.protocol_version {
        None => None,
        Some(0) => Some(""),
        Some(_) => protocol_header_value.as_deref(),
    };

    let info_url = crate::transport::http::smart_info_refs_discovery_url(repo_url);
    let want_v2 = opts.protocol_version.unwrap_or(2) >= 2;
    let (body, final_url) = client
        .get_with_final_url_exact(&info_url, git_protocol)
        .map_err(RemoteError::Library)?;
    let effective_base =
        crate::transport::http::rebased_base_from_redirect(repo_url, final_url.as_deref())
            .unwrap_or_else(|| repo_url.to_owned());
    let (protocol_version, refs, caps, head_symref) =
        if effective_base.trim_end_matches('/') != repo_url.trim_end_matches('/') {
            client.reset_auth_after_redirect_rebase();
            crate::transport::http::discover_upload_pack_for_list_refs(
                client.as_ref(),
                &effective_base,
                git_protocol,
            )
            .map_err(RemoteError::Library)?
        } else {
            crate::transport::http::discover_upload_pack_for_list_refs_from_body(&body)
                .map_err(RemoteError::Library)?
        };

    if protocol_version >= 2 && want_v2 {
        let local_odb = odb.cloned().unwrap_or_else(|| open_odb(local_git_dir));
        let req = build_list_refs_v2_request(&caps, &local_odb, opts)?;
        let post_url = format!("{}/git-upload-pack", effective_base.trim_end_matches('/'));
        let body = client
            .as_ref()
            .post(
                &post_url,
                "application/x-git-upload-pack-request",
                "application/x-git-upload-pack-result",
                &req,
                git_protocol,
            )
            .map_err(RemoteError::Library)?;
        let mut cur = Cursor::new(body);
        parse_list_refs_v2_response(&mut cur, opts, head_symref)
    } else {
        list_refs_v0_advertisement(&refs, head_symref.as_deref(), odb, opts)
    }
}

fn list_refs_from_connection(
    conn: &mut dyn crate::transport::Connection,
    local_git_dir: &Path,
    odb: Option<&Odb>,
    opts: &ListRefsOptions,
) -> RemoteResult<Vec<RemoteRef>> {
    let want_v2 = opts.protocol_version.unwrap_or(2) >= 2;
    if conn.protocol_version() >= 2 && want_v2 {
        list_refs_v2_connection(conn, local_git_dir, odb, opts)
    } else {
        let out =
            list_refs_v0_advertisement(conn.advertised_refs(), conn.head_symref(), odb, opts)?;
        crate::transport::send_client_flush(conn).map_err(RemoteError::Library)?;
        Ok(out)
    }
}

fn open_odb(git_dir: &Path) -> Odb {
    crate::transfer::open_odb(git_dir)
}

fn list_refs_v2_connection(
    conn: &mut dyn crate::transport::Connection,
    local_git_dir: &Path,
    odb: Option<&Odb>,
    opts: &ListRefsOptions,
) -> RemoteResult<Vec<RemoteRef>> {
    let local_odb = odb.cloned().unwrap_or_else(|| open_odb(local_git_dir));
    let req = build_list_refs_v2_request(conn.capabilities(), &local_odb, opts)?;
    conn.writer().write_all(&req).map_err(Error::Io)?;
    conn.writer().flush().map_err(Error::Io)?;
    let discovery_head = conn.head_symref().map(str::to_owned);
    let out = parse_list_refs_v2_response(conn.reader(), opts, discovery_head)?;
    conn.finish_send();
    Ok(out)
}

fn build_list_refs_v2_request(
    server_caps: &[String],
    local_odb: &Odb,
    opts: &ListRefsOptions,
) -> RemoteResult<Vec<u8>> {
    use crate::fetch::v2_object_format;
    use crate::pkt_line;
    use crate::protocol_v2;

    let object_format = v2_object_format(server_caps, local_odb);
    let cap_echo = protocol_v2::cap_lines_for_command_request(server_caps);
    let mut req: Vec<u8> = Vec::new();
    pkt_line::write_line(&mut req, "command=ls-refs").map_err(Error::Io)?;
    if cap_echo.iter().any(|c| c.starts_with("object-format=")) {
        for line in &cap_echo {
            pkt_line::write_line(&mut req, line).map_err(Error::Io)?;
        }
    } else {
        for line in &cap_echo {
            pkt_line::write_line(&mut req, line).map_err(Error::Io)?;
        }
        pkt_line::write_line(&mut req, &format!("object-format={object_format}"))
            .map_err(Error::Io)?;
    }
    pkt_line::write_delim(&mut req).map_err(Error::Io)?;
    if opts.symrefs {
        pkt_line::write_line(&mut req, "symrefs").map_err(Error::Io)?;
        if crate::protocol_v2::server_advertises_ls_refs_unborn(server_caps) {
            pkt_line::write_line(&mut req, "unborn").map_err(Error::Io)?;
        }
    }
    if opts.peel {
        pkt_line::write_line(&mut req, "peel").map_err(Error::Io)?;
    }
    if list_refs_includes_head(opts) {
        pkt_line::write_line(&mut req, "ref-prefix HEAD").map_err(Error::Io)?;
    }
    for prefix in list_ref_prefixes(opts) {
        pkt_line::write_line(&mut req, &format!("ref-prefix {prefix}")).map_err(Error::Io)?;
    }
    pkt_line::write_flush(&mut req).map_err(Error::Io)?;
    Ok(req)
}

fn list_refs_includes_head(opts: &ListRefsOptions) -> bool {
    !opts.heads && !opts.tags && opts.prefixes.is_empty()
}

fn list_ref_prefixes(opts: &ListRefsOptions) -> Vec<String> {
    if !opts.prefixes.is_empty() {
        if opts.heads && !opts.tags {
            return vec!["refs/heads/".to_owned()];
        }
        if opts.tags && !opts.heads {
            return vec!["refs/tags/".to_owned()];
        }
        return vec!["refs/heads/".to_owned(), "refs/tags/".to_owned()];
    }
    let mut out = Vec::new();
    let all = !opts.heads && !opts.tags;
    if all || opts.heads {
        out.push("refs/heads/".to_owned());
    }
    if all || opts.tags {
        out.push("refs/tags/".to_owned());
    }
    out
}

fn parse_list_refs_v2_response(
    reader: &mut dyn Read,
    opts: &ListRefsOptions,
    mut head_symref: Option<String>,
) -> RemoteResult<Vec<RemoteRef>> {
    use crate::fetch::parse_ls_refs_v2_line;
    use crate::pkt_line;

    let mut entries: Vec<RemoteRef> = Vec::new();
    let mut peel_map: std::collections::HashMap<String, ObjectId> =
        std::collections::HashMap::new();
    let mut reader = reader;
    loop {
        match pkt_line::read_packet(&mut reader).map_err(Error::Io)? {
            None | Some(pkt_line::Packet::Flush) | Some(pkt_line::Packet::Delim) => break,
            Some(pkt_line::Packet::ResponseEnd) => break,
            Some(pkt_line::Packet::Data(line)) => {
                let line = line.trim_end_matches('\n');
                pkt_line::reject_err_line(line)?;
                if let Some(t) = crate::fetch::parse_v2_ls_refs_unborn_head_symref(line) {
                    head_symref = Some(t);
                    continue;
                }
                let Some((name, oid, symref_target, peel)) = parse_ls_refs_v2_line(line) else {
                    continue;
                };
                if name.ends_with("^{}") {
                    continue;
                }
                if name == "HEAD" {
                    let sym =
                        symref_target.filter(|t| crate::refs::is_valid_advertised_symref_target(t));
                    if let Some(t) = sym.clone() {
                        head_symref = Some(t);
                    }
                    if list_refs_includes_head(opts) && ref_matches_list_opts("HEAD", opts) {
                        entries.push(RemoteRef {
                            name: "HEAD".to_owned(),
                            oid,
                            symref_target: if opts.symrefs { sym } else { None },
                        });
                    }
                    continue;
                }
                if !crate::refs::is_valid_fetch_advertised_ref(&name) {
                    continue;
                }
                if let Some(p) = peel {
                    peel_map.insert(name.clone(), p);
                }
                let sym =
                    symref_target.filter(|t| crate::refs::is_valid_advertised_symref_target(t));
                entries.push(RemoteRef {
                    name,
                    oid,
                    symref_target: sym,
                });
            }
        }
    }
    finalize_list_refs_output(entries, peel_map, opts, head_symref, None)
}

fn list_refs_v0_advertisement(
    advertised: &[(String, ObjectId)],
    head_symref: Option<&str>,
    odb: Option<&Odb>,
    opts: &ListRefsOptions,
) -> RemoteResult<Vec<RemoteRef>> {
    let mut entries: Vec<RemoteRef> = Vec::new();
    let mut peel_map: std::collections::HashMap<String, ObjectId> =
        std::collections::HashMap::new();
    for (name, oid) in advertised {
        if name.ends_with("^{}") {
            if let Some(base) = name.strip_suffix("^{}") {
                peel_map.insert(base.to_owned(), *oid);
            }
            continue;
        }
        entries.push(RemoteRef {
            name: name.clone(),
            oid: *oid,
            symref_target: None,
        });
    }
    finalize_list_refs_output(entries, peel_map, opts, head_symref.map(str::to_owned), odb)
}

fn finalize_list_refs_output(
    mut entries: Vec<RemoteRef>,
    peel_map: std::collections::HashMap<String, ObjectId>,
    opts: &ListRefsOptions,
    head_symref: Option<String>,
    odb: Option<&Odb>,
) -> RemoteResult<Vec<RemoteRef>> {
    entries.retain(|e| remote_ref_matches_namespace(&e.name, opts));
    entries.retain(|e| patterns_match_ls_remote(&e.name, &opts.prefixes));
    if opts.peel {
        let mut peel_lines: Vec<RemoteRef> = Vec::new();
        for e in &entries {
            if !e.name.starts_with("refs/tags/") {
                continue;
            }
            let peeled = peel_map
                .get(&e.name)
                .copied()
                .or_else(|| odb.and_then(|o| o.peel_tag_target(e.oid).ok().flatten()));
            if let Some(p) = peeled {
                let peel_name = format!("{}^{{}}", e.name);
                if should_emit_peel_line(&peel_name, &opts.prefixes) {
                    peel_lines.push(RemoteRef {
                        name: peel_name,
                        oid: p,
                        symref_target: None,
                    });
                }
            }
        }
        entries.extend(peel_lines);
    }
    if list_refs_includes_head(opts) {
        if let Some(sym) = head_symref {
            if let Some(oid) = entries.iter().find(|e| e.name == sym).map(|e| e.oid) {
                entries.push(RemoteRef {
                    name: "HEAD".to_owned(),
                    oid,
                    symref_target: if opts.symrefs { Some(sym) } else { None },
                });
            }
        }
    }
    let query_options = RefQueryOptions {
        versionsort_suffixes: opts.versionsort_suffixes.clone(),
        ..RefQueryOptions::default()
    };
    sort_remote_refs(
        &mut entries,
        &opts.sort,
        &query_options.versionsort_suffixes,
    );
    if let Some(limit) = opts.limit {
        entries.truncate(limit);
    }
    Ok(entries)
}

/// Filter (heads/tags + ls-remote patterns), sort, and limit an in-memory remote ref list.
///
/// Callers that already applied peel lines and optional `HEAD` pass the full line list here.
///
/// # Panics
///
/// Never panics.
#[must_use]
pub fn query_remote_refs_in_memory(
    mut entries: Vec<RemoteRef>,
    opts: &ListRefsOptions,
    query_options: &RefQueryOptions,
) -> Vec<RemoteRef> {
    entries.retain(|e| remote_ref_matches_namespace(&e.name, opts));
    entries.retain(|e| patterns_match_ls_remote(&e.name, &opts.prefixes));
    sort_remote_refs(
        &mut entries,
        &opts.sort,
        &query_options.versionsort_suffixes,
    );
    if let Some(limit) = opts.limit {
        entries.truncate(limit);
    }
    entries
}

fn should_emit_peel_line(peel_name: &str, patterns: &[String]) -> bool {
    if patterns.is_empty() {
        return true;
    }
    patterns_match_ls_remote(peel_name, patterns)
}

fn sort_remote_refs(entries: &mut [RemoteRef], sort: &RefSort, versionsort_suffixes: &[String]) {
    entries.sort_by(|a, b| {
        let primary = match sort.key {
            SortKey::Name => a.name.cmp(&b.name),
            SortKey::Version => compare_version_names(&a.name, &b.name, versionsort_suffixes),
            SortKey::Date => a.name.cmp(&b.name),
        };
        let primary = if sort.reverse {
            primary.reverse()
        } else {
            primary
        };
        if primary != std::cmp::Ordering::Equal {
            return primary;
        }
        a.name.cmp(&b.name)
    });
}

fn remote_ref_matches_namespace(refname: &str, opts: &ListRefsOptions) -> bool {
    if opts.heads || opts.tags {
        let is_branch = opts.heads && refname.starts_with("refs/heads/");
        let is_tag = opts.tags && refname.starts_with("refs/tags/");
        if refname == "HEAD" {
            return false;
        }
        if refname.ends_with("^{}") {
            return is_tag;
        }
        if !is_branch && !is_tag {
            return false;
        }
    }
    true
}

fn ref_matches_list_opts(refname: &str, opts: &ListRefsOptions) -> bool {
    remote_ref_matches_namespace(refname, opts) && patterns_match_ls_remote(refname, &opts.prefixes)
}

/// Returns `true` when `refname` matches one of `patterns`, or when `patterns` is empty.
#[must_use]
pub fn ref_matches_ls_remote_patterns(refname: &str, patterns: &[String]) -> bool {
    patterns_match_ls_remote(refname, patterns)
}

/// List references from a on-disk git directory (local / `file://` remotes).
///
/// # Errors
fn list_refs_from_bundle(path: &Path, opts: &ListRefsOptions) -> RemoteResult<Vec<RemoteRef>> {
    let bundle = crate::bundle::Bundle::open(path)
        .map_err(|e| RemoteError::Library(Error::Message(e.to_string())))?;
    let mut out = Vec::new();
    for (name, oid) in &bundle.header().refs {
        if !ref_matches_list_opts(name, opts) {
            continue;
        }
        if name != "HEAD" && !crate::refs::is_valid_fetch_advertised_ref(name) {
            continue;
        }
        out.push(RemoteRef {
            name: name.clone(),
            oid: *oid,
            symref_target: None,
        });
    }
    sort_remote_refs(&mut out, &opts.sort, &opts.versionsort_suffixes);
    if let Some(limit) = opts.limit {
        out.truncate(limit);
    }
    Ok(out)
}

/// List refs from a on-disk git directory using explicit scan prefixes (fetch).
pub(crate) fn list_refs_from_git_dir_for_fetch(
    git_dir: &Path,
    odb: Option<&Odb>,
    refspecs: &[String],
) -> RemoteResult<Vec<RemoteRef>> {
    let prefixes = crate::fetch::fetch_ref_list_prefixes(refspecs);
    let opts = ListRefsOptions {
        symrefs: true,
        ..Default::default()
    };
    list_refs_from_git_dir_with_scan_prefixes(git_dir, odb, &opts, &prefixes)
}

///
/// Returns [`RemoteError::Library`] on I/O or ref resolution failures.
pub fn list_refs_from_git_dir(
    git_dir: &Path,
    odb: Option<&Odb>,
    opts: &ListRefsOptions,
) -> RemoteResult<Vec<RemoteRef>> {
    list_refs_from_git_dir_with_scan_prefixes(git_dir, odb, opts, &list_ref_prefixes(opts))
}

fn list_refs_from_git_dir_with_scan_prefixes(
    git_dir: &Path,
    odb: Option<&Odb>,
    opts: &ListRefsOptions,
    scan_prefixes: &[String],
) -> RemoteResult<Vec<RemoteRef>> {
    let mut lazy_odb: Option<Odb> = None;

    let mut entries = Vec::new();
    if list_refs_includes_head(opts) && ref_matches_list_opts("HEAD", opts) {
        let symref_target = if opts.symrefs {
            crate::refs::read_symbolic_ref(git_dir, "HEAD")
                .ok()
                .flatten()
        } else {
            None
        };
        match crate::refs::resolve_ref(git_dir, "HEAD") {
            Ok(head_oid) => {
                entries.push(RemoteRef {
                    name: "HEAD".to_owned(),
                    oid: head_oid,
                    symref_target,
                });
            }
            Err(_) if symref_target.is_some() => {
                entries.push(RemoteRef {
                    name: "HEAD".to_owned(),
                    oid: ObjectId::zero(),
                    symref_target,
                });
            }
            Err(_) => {}
        }
    }

    for prefix in scan_prefixes {
        let listed = crate::refs::list_refs(git_dir, prefix).map_err(RemoteError::Library)?;
        for (name, oid) in listed {
            if let Some(branch_tail) = name.strip_prefix("refs/heads/") {
                if branch_tail.starts_with("refs/") {
                    continue;
                }
            }
            if !ref_matches_list_opts(&name, opts) {
                continue;
            }
            entries.push(RemoteRef {
                name: name.clone(),
                oid,
                symref_target: None,
            });
            if opts.peel && name.starts_with("refs/tags/") {
                let peel_name = format!("{name}^{{}}");
                if should_emit_peel_line(&peel_name, &opts.prefixes) {
                    let peel_odb = match odb {
                        Some(o) => o,
                        None => lazy_odb.get_or_insert_with(|| {
                            Odb::new(&git_dir.join("objects"))
                                .with_config_git_dir(git_dir.to_path_buf())
                        }),
                    };
                    if let Ok(Some(peeled)) = peel_odb.peel_tag_target(oid) {
                        entries.push(RemoteRef {
                            name: peel_name,
                            oid: peeled,
                            symref_target: None,
                        });
                    }
                }
            }
        }
    }

    sort_remote_refs(&mut entries, &opts.sort, &opts.versionsort_suffixes);
    if let Some(limit) = opts.limit {
        entries.truncate(limit);
    }
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn remote_ref(name: &str, hex_digit: char) -> RemoteRef {
        RemoteRef {
            name: name.to_owned(),
            oid: ObjectId::from_hex(&hex_digit.to_string().repeat(40)).unwrap(),
            symref_target: None,
        }
    }

    #[test]
    fn missing_urls_name_the_remote() {
        let err = parse_remote_url("  ").expect_err("empty");
        assert!(matches!(err, RemoteError::NoUrl { remote: None }));
        assert_eq!(err.to_string(), "remote URL is empty");

        let config = ConfigSet::new();
        let err = Remote::from_config(&config, "upstream").expect_err("no url");
        assert!(matches!(&err, RemoteError::NoUrl { remote: Some(name) } if name == "upstream"));
        assert_eq!(err.to_string(), "remote 'upstream' has no URL configured");
    }

    #[test]
    fn fold_peeled_refs_attaches_peeled_lines_and_keeps_order() {
        let mut head = remote_ref("HEAD", '1');
        head.symref_target = Some("refs/heads/main".to_owned());
        let refs = vec![
            head,
            remote_ref("refs/heads/main", '1'),
            remote_ref("refs/tags/light", '1'),
            remote_ref("refs/tags/v1", '2'),
            remote_ref("refs/tags/v1^{}", '1'),
        ];
        let folded = fold_peeled_refs(&refs);
        let names: Vec<&str> = folded.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(
            names,
            ["HEAD", "refs/heads/main", "refs/tags/light", "refs/tags/v1"]
        );
        assert_eq!(folded[0].symref_target.as_deref(), Some("refs/heads/main"));
        assert_eq!(folded[2].peeled, None);
        assert_eq!(folded[3].oid, refs[3].oid);
        assert_eq!(folded[3].peeled, Some(refs[4].oid));
    }

    #[test]
    fn peeled_tag_only_matches_peel_suffix() {
        assert_eq!(
            remote_ref("refs/tags/v1^{}", '1').peeled_tag(),
            Some("refs/tags/v1")
        );
        assert_eq!(remote_ref("refs/tags/v1", '1').peeled_tag(), None);
    }

    #[test]
    fn remote_url_parse_table() {
        assert!(matches!(
            RemoteUrl::try_from("/abs/path").unwrap(),
            RemoteUrl::Local(_)
        ));
        assert!(matches!(
            RemoteUrl::try_from("file:///tmp/r.git").unwrap(),
            RemoteUrl::File(_)
        ));
        assert!(matches!(
            RemoteUrl::try_from("git://host/repo").unwrap(),
            RemoteUrl::Git(_)
        ));
        assert!(matches!(
            RemoteUrl::try_from("git@host:repo.git").unwrap(),
            RemoteUrl::Ssh(_)
        ));
        assert!(matches!(
            RemoteUrl::try_from("ssh://git@host/repo").unwrap(),
            RemoteUrl::Ssh(_)
        ));
        assert!(matches!(
            RemoteUrl::try_from("https://host/r.git").unwrap(),
            RemoteUrl::Https(_)
        ));
        assert!(matches!(
            RemoteUrl::try_from("ftp://x"),
            Err(RemoteError::UnsupportedScheme)
        ));
    }

    #[test]
    fn pattern_matches_empty_allows_all() {
        assert!(ref_matches_ls_remote_patterns("refs/heads/main", &[]));
        assert!(ref_matches_ls_remote_patterns("HEAD", &[]));
    }

    #[test]
    fn pattern_matches_exact() {
        let pats = vec!["HEAD".to_owned()];
        assert!(ref_matches_ls_remote_patterns("HEAD", &pats));
        assert!(!ref_matches_ls_remote_patterns("refs/heads/main", &pats));
    }

    #[test]
    fn pattern_matches_suffix_component() {
        let pats = vec!["main".to_owned()];
        assert!(ref_matches_ls_remote_patterns("refs/heads/main", &pats));
        assert!(!ref_matches_ls_remote_patterns("refs/heads/notmain", &pats));
        assert!(!ref_matches_ls_remote_patterns("main-branch", &pats));
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn query_remote_refs_in_memory_empty_pattern_matches_nothing() {
        let entries = vec![
            RemoteRef {
                name: "refs/heads/main".into(),
                oid: ObjectId::from_hex("1".repeat(40).as_str()).unwrap(),
                symref_target: None,
            },
            RemoteRef {
                name: "refs/tags/v1".into(),
                oid: ObjectId::from_hex("2".repeat(40).as_str()).unwrap(),
                symref_target: None,
            },
        ];
        let opts = ListRefsOptions {
            prefixes: vec![String::new()],
            ..Default::default()
        };
        let out = query_remote_refs_in_memory(entries, &opts, &RefQueryOptions::default());
        assert!(out.is_empty());
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn query_remote_refs_in_memory_version_sort_and_limit() {
        let entries = vec![
            RemoteRef {
                name: "refs/tags/v1.10".into(),
                oid: ObjectId::from_hex("1".repeat(40).as_str()).unwrap(),
                symref_target: None,
            },
            RemoteRef {
                name: "refs/tags/v1.2".into(),
                oid: ObjectId::from_hex("2".repeat(40).as_str()).unwrap(),
                symref_target: None,
            },
            RemoteRef {
                name: "refs/tags/v1.1".into(),
                oid: ObjectId::from_hex("3".repeat(40).as_str()).unwrap(),
                symref_target: None,
            },
        ];
        let opts = ListRefsOptions {
            sort: RefSort {
                key: SortKey::Version,
                reverse: false,
            },
            limit: Some(2),
            ..Default::default()
        };
        let out = query_remote_refs_in_memory(entries, &opts, &RefQueryOptions::default());
        assert_eq!(
            out.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(),
            vec!["refs/tags/v1.1", "refs/tags/v1.2"]
        );
    }

    #[test]
    fn list_opts_heads_excludes_head() {
        let heads_only = super::ListRefsOptions {
            heads: true,
            ..Default::default()
        };
        assert!(!super::ref_matches_list_opts("HEAD", &heads_only));
        assert!(super::ref_matches_list_opts("refs/heads/main", &heads_only));
        assert!(!super::ref_matches_list_opts("refs/tags/v1", &heads_only));
    }

    #[test]
    fn insteadof_rewrite_applied_in_from_config() {
        let mut cfg = ConfigSet::new();
        cfg.add_command_override("url.https://github.com/.insteadOf", "gh:")
            .unwrap();
        cfg.add_command_override("remote.origin.url", "gh:org/repo.git")
            .unwrap();
        let remote = Remote::from_config(&cfg, "origin").unwrap();
        assert!(matches!(remote.fetch_url(), RemoteUrl::Https(_)));
    }

    #[test]
    fn push_insteadof_applied_when_pushurl_absent() {
        let mut cfg = ConfigSet::new();
        cfg.add_command_override("url.ssh://push.example/.pushInsteadOf", "short:")
            .unwrap();
        cfg.add_command_override("remote.origin.url", "short:org/repo.git")
            .unwrap();
        let remote = Remote::from_config(&cfg, "origin").unwrap();
        assert_eq!(
            remote.push_url().to_url_string(),
            "ssh://push.example/org/repo.git"
        );
        assert_eq!(remote.fetch_url().to_url_string(), "short:org/repo.git");
    }

    #[test]
    fn list_ref_prefixes_broad_for_ls_remote_patterns() {
        let opts = super::ListRefsOptions {
            prefixes: vec!["main".to_owned()],
            ..Default::default()
        };
        let prefixes = super::list_ref_prefixes(&opts);
        assert_eq!(
            prefixes,
            vec!["refs/heads/".to_owned(), "refs/tags/".to_owned()]
        );
        assert!(super::ref_matches_list_opts("refs/heads/main", &opts));
        assert!(!super::ref_matches_list_opts("refs/heads/topic", &opts));
    }
}
