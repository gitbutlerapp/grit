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
use crate::credentials::HelperCredentialProvider;
use crate::error::{Error, Result};
use crate::fetch::{fetch_remote, Progress};
use crate::objects::{ObjectId, ObjectKind};
use crate::odb::Odb;
use crate::push::push_remote;
use crate::repo::Repository;
use crate::transfer::{
    fetch_local, push_local, FetchOptions, FetchOutcome, PushOptions, PushOutcome, PushRefSpec,
};
use crate::transport::http::{http_fetch, HttpClient};
use crate::transport::{ConnectOptions, GitDaemonTransport, Service, SshTransport, Transport};
use crate::transport_path::{
    is_local_path_remote_url, resolve_local_remote_git_dir, url_is_local_not_ssh,
};
use crate::url_rewrite;

#[cfg(feature = "http-ureq")]
use crate::transport::http::ureq_client::UreqHttpClient;

/// HTTP client types for non-git HTTP (e.g. OAuth) without importing [`crate::transport`] from binaries.
#[cfg(feature = "http-ureq")]
pub mod http_client {
    pub use crate::transport::http::ureq_client::UreqHttpClient;
    pub use crate::transport::http::HttpClient;
}

type RemoteResult<T> = std::result::Result<T, RemoteError>;

/// Default remote name when none is configured (`origin`).
pub const DEFAULT_REMOTE: &str = "origin";

/// Errors specific to remote URL resolution and dispatch.
#[derive(Debug, Error)]
pub enum RemoteError {
    /// A remote name has no `remote.<name>.url` configured.
    #[error("remote has no configured URL")]
    NoUrl,
    /// The URL uses a scheme grit does not support for remotes.
    #[error("unsupported remote URL scheme")]
    UnsupportedScheme,
    /// A URL or path failed to parse.
    #[error("{0}")]
    InvalidUrl(String),
    /// Smart HTTP requires the `http-ureq` feature or an injected HTTP client factory.
    #[error("HTTP remotes require the http-ureq feature or an HttpClientFactory")]
    HttpUnavailable,
    /// An underlying library error.
    #[error(transparent)]
    Library(#[from] Error),
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
            Self::Local(_) | Self::File(_) => None,
        }
    }

    /// Original configured string form (paths normalized for display).
    #[must_use]
    pub fn to_url_string(&self) -> String {
        match self {
            Self::Local(p) | Self::File(p) => p.to_string_lossy().into_owned(),
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
        return Err(RemoteError::NoUrl);
    }
    if url.starts_with("file://") {
        let path = crate::transport_path::file_url_to_local_path(url)
            .map_err(|e| RemoteError::InvalidUrl(e.to_string()))?;
        return Ok(RemoteUrl::File(PathBuf::from(path)));
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
        return Ok(RemoteUrl::Local(PathBuf::from(url)));
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
}

/// Builds HTTP clients for smart-HTTP remotes.
pub trait HttpClientFactory: Send + Sync {
    /// Create a client honoring repository HTTP/credential config.
    ///
    /// # Errors
    ///
    /// Returns an error when the client cannot be constructed.
    fn create(&self, config: &ConfigSet) -> RemoteResult<Box<dyn HttpClient>>;
}

/// Default [`HttpClientFactory`] using `UreqHttpClient` (`http-ureq` feature).
#[cfg(feature = "http-ureq")]
#[derive(Debug, Default, Clone, Copy)]
pub struct DefaultHttpClientFactory;

#[cfg(feature = "http-ureq")]
impl HttpClientFactory for DefaultHttpClientFactory {
    fn create(&self, config: &ConfigSet) -> RemoteResult<Box<dyn HttpClient>> {
        let provider = Box::new(HelperCredentialProvider::new(config.clone()));
        let client = UreqHttpClient::from_config(config)
            .map_err(|e| RemoteError::InvalidUrl(e.to_string()))?
            .with_credential_provider(provider);
        Ok(Box::new(client))
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
            .ok_or(RemoteError::NoUrl)?;
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

    /// Build a remote from a literal URL with the default fetch refspec layout for `origin`.
    ///
    /// # Errors
    ///
    /// Returns an error when `url` is empty or unsupported.
    pub fn from_url(url: &str) -> RemoteResult<Self> {
        Ok(Self {
            name: None,
            fetch_url: RemoteUrl::try_from(url)?,
            push_url: None,
            fetch_refspecs: vec!["+refs/heads/*:refs/remotes/origin/*".to_owned()],
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
    /// Returns [`RemoteError`] or transport errors on failure.
    pub fn fetch(
        &self,
        repo: &Repository,
        mut opts: FetchOptions,
        progress: &mut dyn Progress,
        http_factory: Option<&dyn HttpClientFactory>,
    ) -> RemoteResult<FetchOutcome> {
        if opts.refspecs.is_empty() {
            opts.refspecs = self.fetch_refspecs.clone();
        }
        if opts.diagnostics.is_none() {
            opts.diagnostics = Some(repo.diagnostics());
            opts.network_trace = repo.network_trace_enabled();
        }
        self.dispatch_fetch(
            repo,
            &self.fetch_url,
            &opts,
            progress,
            http_factory,
            ConnectOptions {
                protocol_version: 2,
                diagnostics: opts.diagnostics.clone(),
                network_trace: opts.network_trace,
                ..Default::default()
            },
        )
    }

    /// Push `refs` to this remote from `repo`.
    ///
    /// # Errors
    ///
    /// Returns [`RemoteError`] or transport errors on failure.
    pub fn push(
        &self,
        repo: &Repository,
        refs: &[PushRefSpec],
        mut opts: PushOptions,
        progress: &mut dyn Progress,
        http_factory: Option<&dyn HttpClientFactory>,
    ) -> RemoteResult<PushOutcome> {
        if opts.tracking_remote.is_none() {
            opts.tracking_remote = self.name.clone();
        }
        self.dispatch_push(
            repo,
            self.push_url(),
            refs,
            &opts,
            progress,
            http_factory,
            ConnectOptions {
                protocol_version: 0,
                diagnostics: Some(repo.diagnostics()),
                network_trace: repo.network_trace_enabled(),
                ..Default::default()
            },
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
            RemoteUrl::Local(_) | RemoteUrl::File(_) => {
                let remote_git = local_git_dir_from_url(&self.fetch_url, git_dir, work_tree)?;
                let remote_odb =
                    Odb::new(&remote_git.join("objects")).with_config_git_dir(remote_git.clone());
                list_refs_from_git_dir(&remote_git, &remote_odb, opts)
            }
            wire => self.list_refs_wire(wire, git_dir, odb.as_ref(), opts, http_factory, repo),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn dispatch_fetch(
        &self,
        repo: &Repository,
        url: &RemoteUrl,
        opts: &FetchOptions,
        progress: &mut dyn Progress,
        http_factory: Option<&dyn HttpClientFactory>,
        connect: ConnectOptions,
    ) -> RemoteResult<FetchOutcome> {
        match url {
            RemoteUrl::Local(_) | RemoteUrl::File(_) => {
                let remote_git =
                    local_git_dir_from_url(url, &repo.git_dir, repo.work_tree.as_deref())?;
                Ok(fetch_local(&repo.git_dir, &remote_git, opts)?)
            }
            RemoteUrl::Http(u) | RemoteUrl::Https(u) => {
                let client = http_client(http_factory, repo)?;
                Ok(http_fetch(&*client, &repo.git_dir, u, opts, progress)?)
            }
            RemoteUrl::Git(u) => {
                let mut conn =
                    GitDaemonTransport::new().connect(u, Service::UploadPack, &connect)?;
                Ok(fetch_remote(&repo.git_dir, &mut *conn, opts, progress)?)
            }
            RemoteUrl::Ssh(u) => {
                let transport = ssh_transport(Some(repo));
                let mut conn = transport.connect(u, Service::UploadPack, &connect)?;
                Ok(fetch_remote(&repo.git_dir, &mut *conn, opts, progress)?)
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
        progress: &mut dyn Progress,
        http_factory: Option<&dyn HttpClientFactory>,
        connect: ConnectOptions,
    ) -> RemoteResult<PushOutcome> {
        match url {
            RemoteUrl::Local(_) | RemoteUrl::File(_) => {
                let remote_git =
                    local_git_dir_from_url(url, &repo.git_dir, repo.work_tree.as_deref())?;
                Ok(push_local(&repo.git_dir, &remote_git, refs, opts)?)
            }
            RemoteUrl::Http(u) | RemoteUrl::Https(u) => {
                let client = http_client(http_factory, repo)?;
                Ok(crate::push::push_http(
                    client.as_ref(),
                    &repo.git_dir,
                    u,
                    refs,
                    opts,
                    progress,
                )?)
            }
            RemoteUrl::Git(u) => {
                let mut conn =
                    GitDaemonTransport::new().connect(u, Service::ReceivePack, &connect)?;
                Ok(push_remote(
                    &repo.git_dir,
                    &mut *conn,
                    refs,
                    opts,
                    progress,
                )?)
            }
            RemoteUrl::Ssh(u) => {
                let transport = ssh_transport(Some(repo));
                let mut conn = transport.connect(u, Service::ReceivePack, &connect)?;
                Ok(push_remote(
                    &repo.git_dir,
                    &mut *conn,
                    refs,
                    opts,
                    progress,
                )?)
            }
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
        let requested = opts.protocol_version.unwrap_or(2);
        let connect_opts = ConnectOptions {
            protocol_version: requested,
            ..Default::default()
        };
        match url {
            RemoteUrl::Http(_) | RemoteUrl::Https(_) => {
                list_refs_http(wire, local_git_dir, odb, opts, http_factory, repo)
            }
            RemoteUrl::Git(_) => {
                let mut conn =
                    GitDaemonTransport::new().connect(wire, Service::UploadPack, &connect_opts)?;
                list_refs_from_connection(&mut *conn, local_git_dir, odb, opts)
            }
            RemoteUrl::Ssh(_) => {
                let transport = ssh_transport(repo);
                let mut conn = transport.connect(wire, Service::UploadPack, &connect_opts)?;
                list_refs_from_connection(&mut *conn, local_git_dir, odb, opts)
            }
            RemoteUrl::Local(_) | RemoteUrl::File(_) => unreachable!(),
        }
    }
}

fn repo_context(repo: Option<&Repository>) -> (&Path, Option<&Path>, Option<Odb>) {
    match repo {
        Some(r) => (
            r.git_dir.as_path(),
            r.work_tree.as_deref(),
            Some(r.odb.clone()),
        ),
        None => (Path::new("."), None, None),
    }
}

fn local_git_dir_from_url(
    url: &RemoteUrl,
    git_dir: &Path,
    work_tree: Option<&Path>,
) -> RemoteResult<PathBuf> {
    let s = url.to_url_string();
    Ok(resolve_local_remote_git_dir(&s, git_dir, work_tree))
}

fn ssh_transport(repo: Option<&Repository>) -> SshTransport {
    let mut transport = SshTransport::new();
    if let Some(r) = repo {
        transport = transport
            .with_command_runner(r.command_runner())
            .with_ssh_environment(std::sync::Arc::new(r.environment().clone()));
    }
    transport
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
        DefaultHttpClientFactory.create(config.as_ref())
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
                DefaultHttpClientFactory.create(config.as_ref())?
            }
            #[cfg(not(feature = "http-ureq"))]
            {
                return Err(RemoteError::HttpUnavailable);
            }
        }
    };

    let info_url = crate::transport::http::smart_info_refs_discovery_url(repo_url);
    let git_protocol = Some("version=2");
    let (body, final_url) = client
        .get_with_final_url(&info_url, git_protocol)
        .map_err(RemoteError::Library)?;
    let effective_base =
        crate::transport::http::rebased_base_from_redirect(repo_url, final_url.as_deref())
            .unwrap_or_else(|| repo_url.to_owned());
    let (protocol_version, refs, caps, head_symref) = if effective_base.trim_end_matches('/')
        != repo_url.trim_end_matches('/')
    {
        client.reset_auth_after_redirect_rebase();
        crate::transport::http::discover_upload_pack(client.as_ref(), &effective_base, git_protocol)
            .map_err(RemoteError::Library)?
    } else {
        crate::transport::http::discover_upload_pack_from_body(&body)
            .map_err(RemoteError::Library)?
    };

    if protocol_version >= 2 && opts.protocol_version.unwrap_or(2) >= 2 {
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
        parse_list_refs_v2_response(&mut cur, opts)
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
        list_refs_v0_advertisement(conn.advertised_refs(), conn.head_symref(), odb, opts)
    }
}

fn open_odb(git_dir: &Path) -> Odb {
    Odb::new(&git_dir.join("objects")).with_config_git_dir(git_dir.to_path_buf())
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
    conn.finish_send();
    parse_list_refs_v2_response(conn.reader(), opts)
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
    if opts.heads && !opts.tags {
        return vec!["refs/heads/".to_owned()];
    }
    if opts.tags && !opts.heads {
        return vec!["refs/tags/".to_owned()];
    }
    if !opts.prefixes.is_empty() {
        return v2_ref_prefixes_from_ls_remote_patterns(&opts.prefixes);
    }
    vec!["refs/".to_owned()]
}

/// Map `git ls-remote`-style patterns to v2 `ref-prefix` lines (broad enough to fetch, then filter client-side).
fn v2_ref_prefixes_from_ls_remote_patterns(patterns: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let push_unique = |out: &mut Vec<String>, value: &str| {
        if !out.iter().any(|v| v == value) {
            out.push(value.to_owned());
        }
    };
    for pat in patterns {
        let pat = pat.trim();
        if pat.is_empty() {
            continue;
        }
        if pat == "HEAD" {
            push_unique(&mut out, "HEAD");
            continue;
        }
        if let Some(star) = pat.find('*') {
            let prefix = &pat[..star];
            if prefix.is_empty() {
                push_unique(&mut out, "refs/");
                continue;
            }
            if prefix.starts_with("refs/") {
                push_unique(&mut out, prefix);
            } else {
                push_unique(&mut out, &format!("refs/heads/{prefix}"));
            }
            continue;
        }
        if pat.starts_with("refs/") {
            push_unique(&mut out, pat);
            continue;
        }
        push_unique(&mut out, "refs/heads/");
        push_unique(&mut out, "refs/tags/");
    }
    out
}

fn parse_list_refs_v2_response(
    reader: &mut dyn Read,
    opts: &ListRefsOptions,
) -> RemoteResult<Vec<RemoteRef>> {
    use crate::fetch::parse_ls_refs_v2_line;
    use crate::pkt_line;

    let mut entries: Vec<RemoteRef> = Vec::new();
    let mut peel_map: std::collections::HashMap<String, ObjectId> =
        std::collections::HashMap::new();
    let mut head_symref: Option<String> = None;
    let mut head_oid: Option<ObjectId> = None;
    let mut reader = reader;
    loop {
        match pkt_line::read_packet(&mut reader).map_err(Error::Io)? {
            None | Some(pkt_line::Packet::Flush) | Some(pkt_line::Packet::Delim) => break,
            Some(pkt_line::Packet::ResponseEnd) => break,
            Some(pkt_line::Packet::Data(line)) => {
                let line = line.trim_end_matches('\n');
                if let Some(msg) = line.strip_prefix("ERR ") {
                    return Err(RemoteError::Library(Error::Message(format!(
                        "remote error: {}",
                        msg.trim_end()
                    ))));
                }
                let Some((name, oid, symref_target, peel)) = parse_ls_refs_v2_line(line) else {
                    continue;
                };
                if name.ends_with("^{}") {
                    continue;
                }
                if name == "HEAD" {
                    head_oid = Some(oid);
                    if let Some(t) =
                        symref_target.filter(|t| crate::refs::is_valid_advertised_symref_target(t))
                    {
                        head_symref = Some(t);
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
    finalize_list_refs_output(entries, peel_map, opts, head_symref, head_oid, None)
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
    finalize_list_refs_output(
        entries,
        peel_map,
        opts,
        head_symref.map(str::to_owned),
        None,
        odb,
    )
}

fn finalize_list_refs_output(
    mut entries: Vec<RemoteRef>,
    peel_map: std::collections::HashMap<String, ObjectId>,
    opts: &ListRefsOptions,
    head_symref: Option<String>,
    head_oid: Option<ObjectId>,
    odb: Option<&Odb>,
) -> RemoteResult<Vec<RemoteRef>> {
    entries.retain(|e| ref_matches_list_opts(&e.name, opts));
    if opts.peel {
        let mut peel_lines: Vec<RemoteRef> = Vec::new();
        for e in &entries {
            if !e.name.starts_with("refs/tags/") {
                continue;
            }
            let peeled = peel_map
                .get(&e.name)
                .copied()
                .or_else(|| odb.and_then(|o| peel_tag(o, &e.oid)));
            if let Some(p) = peeled {
                let peel_name = format!("{}^{{}}", e.name);
                if ref_matches_list_opts(&peel_name, opts) {
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
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    if list_refs_includes_head(opts) && ref_matches_list_opts("HEAD", opts) {
        let resolved = head_oid.or_else(|| {
            head_symref
                .as_ref()
                .and_then(|sym| entries.iter().find(|e| e.name == *sym).map(|e| e.oid))
        });
        if let Some(oid) = resolved {
            entries.insert(
                0,
                RemoteRef {
                    name: "HEAD".to_owned(),
                    oid,
                    symref_target: if opts.symrefs { head_symref } else { None },
                },
            );
        }
    }
    Ok(entries)
}

fn ref_matches_list_opts(refname: &str, opts: &ListRefsOptions) -> bool {
    if opts.heads || opts.tags {
        let is_branch = opts.heads && refname.starts_with("refs/heads/");
        let is_tag = opts.tags && refname.starts_with("refs/tags/");
        if !is_branch && !is_tag {
            return false;
        }
    }
    ref_matches_ls_remote_patterns(refname, &opts.prefixes)
}

/// Returns `true` when `refname` matches one of `patterns`, or when `patterns` is empty.
#[must_use]
pub fn ref_matches_ls_remote_patterns(refname: &str, patterns: &[String]) -> bool {
    pattern_matches(refname, patterns)
}

fn pattern_matches(refname: &str, patterns: &[String]) -> bool {
    if patterns.is_empty() {
        return true;
    }
    let path = format!("/{refname}");
    patterns.iter().any(|pat| {
        let full = format!("*/{pat}");
        glob_match(&full, &path)
    })
}

fn glob_match(pattern: &str, text: &str) -> bool {
    let pat: Vec<char> = pattern.chars().collect();
    let txt: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0, 0);
    let (mut star_pi, mut star_ti) = (usize::MAX, 0);
    while ti < txt.len() {
        if pi < pat.len() && (pat[pi] == '?' || pat[pi] == txt[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < pat.len() && pat[pi] == '*' {
            star_pi = pi;
            star_ti = ti;
            pi += 1;
        } else if star_pi != usize::MAX {
            pi = star_pi + 1;
            star_ti += 1;
            ti = star_ti;
        } else {
            return false;
        }
    }
    while pi < pat.len() && pat[pi] == '*' {
        pi += 1;
    }
    pi == pat.len()
}

/// List references from a on-disk git directory (local / `file://` remotes).
///
/// # Errors
///
/// Returns [`RemoteError::Library`] on I/O or ref resolution failures.
pub fn list_refs_from_git_dir(
    git_dir: &Path,
    odb: &Odb,
    opts: &ListRefsOptions,
) -> RemoteResult<Vec<RemoteRef>> {
    use std::collections::BTreeMap;

    let mut entries = Vec::new();
    if list_refs_includes_head(opts) {
        if let Ok(head_oid) = crate::refs::resolve_ref(git_dir, "HEAD") {
            let symref_target = if opts.symrefs {
                crate::refs::read_symbolic_ref(git_dir, "HEAD")
                    .ok()
                    .flatten()
            } else {
                None
            };
            if ref_matches_list_opts("HEAD", opts) {
                entries.push(RemoteRef {
                    name: "HEAD".to_owned(),
                    oid: head_oid,
                    symref_target,
                });
            }
        }
    }

    let refs_dir_root = resolve_common_git_dir(git_dir).unwrap_or_else(|| git_dir.to_path_buf());
    let mut all_refs: BTreeMap<String, ObjectId> = BTreeMap::new();
    collect_loose_refs(
        &refs_dir_root,
        &refs_dir_root.join("refs"),
        "refs",
        &mut all_refs,
    )
    .map_err(RemoteError::Library)?;
    for (name, oid) in read_packed_refs(&refs_dir_root).map_err(RemoteError::Library)? {
        all_refs.entry(name).or_insert(oid);
    }

    for (name, oid) in &all_refs {
        if let Some(branch_tail) = name.strip_prefix("refs/heads/") {
            if branch_tail.starts_with("refs/") {
                continue;
            }
        }
        if !ref_matches_list_opts(name, opts) {
            continue;
        }
        entries.push(RemoteRef {
            name: name.clone(),
            oid: *oid,
            symref_target: None,
        });
        if opts.peel && name.starts_with("refs/tags/") {
            let peel_name = format!("{name}^{{}}");
            if ref_matches_list_opts(&peel_name, opts) {
                if let Some(peeled) = peel_tag(odb, oid) {
                    entries.push(RemoteRef {
                        name: peel_name,
                        oid: peeled,
                        symref_target: None,
                    });
                }
            }
        }
    }

    entries.sort_by(|a, b| {
        if a.name == "HEAD" {
            std::cmp::Ordering::Less
        } else if b.name == "HEAD" {
            std::cmp::Ordering::Greater
        } else {
            a.name.cmp(&b.name)
        }
    });
    Ok(entries)
}

fn resolve_common_git_dir(git_dir: &Path) -> Option<PathBuf> {
    let raw = std::fs::read_to_string(git_dir.join("commondir")).ok()?;
    let rel = raw.trim();
    if rel.is_empty() {
        return None;
    }
    let candidate = if Path::new(rel).is_absolute() {
        PathBuf::from(rel)
    } else {
        git_dir.join(rel)
    };
    candidate.canonicalize().ok()
}

fn collect_loose_refs(
    git_dir: &Path,
    path: &Path,
    relative: &str,
    out: &mut std::collections::BTreeMap<String, ObjectId>,
) -> Result<()> {
    use std::fs;
    use std::io;

    let read_dir = match fs::read_dir(path) {
        Ok(rd) => rd,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(Error::Io(e)),
    };
    for entry in read_dir {
        let entry = entry?;
        let file_name = entry.file_name().to_string_lossy().to_string();
        let next_relative = format!("{relative}/{file_name}");
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            collect_loose_refs(git_dir, &entry.path(), &next_relative, out)?;
        } else if file_type.is_file() {
            if let Ok(oid) = crate::refs::resolve_ref(git_dir, &next_relative) {
                out.insert(next_relative, oid);
            }
        }
    }
    Ok(())
}

fn read_packed_refs(git_dir: &Path) -> Result<Vec<(String, ObjectId)>> {
    use std::fs;
    use std::io;

    let path = git_dir.join("packed-refs");
    let text = match fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(Error::Io(e)),
    };
    let mut entries = Vec::new();
    for line in text.lines() {
        if line.is_empty() || line.starts_with('#') || line.starts_with('^') {
            continue;
        }
        let mut parts = line.split_whitespace();
        let Some(oid_str) = parts.next() else {
            continue;
        };
        let Some(name) = parts.next() else {
            continue;
        };
        if let Ok(oid) = oid_str.parse::<ObjectId>() {
            entries.push((name.to_owned(), oid));
        }
    }
    Ok(entries)
}

fn peel_tag(odb: &Odb, oid: &ObjectId) -> Option<ObjectId> {
    let obj = odb.read(oid).ok()?;
    if obj.kind != ObjectKind::Tag {
        return None;
    }
    let text = std::str::from_utf8(&obj.data).ok()?;
    for line in text.lines() {
        if let Some(target) = line.strip_prefix("object ") {
            return target.trim().parse::<ObjectId>().ok();
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(super::pattern_matches("refs/heads/main", &[]));
        assert!(super::pattern_matches("HEAD", &[]));
    }

    #[test]
    fn pattern_matches_exact() {
        let pats = vec!["HEAD".to_owned()];
        assert!(super::pattern_matches("HEAD", &pats));
        assert!(!super::pattern_matches("refs/heads/main", &pats));
    }

    #[test]
    fn pattern_matches_suffix_component() {
        let pats = vec!["main".to_owned()];
        assert!(super::pattern_matches("refs/heads/main", &pats));
        assert!(!super::pattern_matches("refs/heads/notmain", &pats));
        assert!(!super::pattern_matches("main-branch", &pats));
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

    #[test]
    fn list_ref_prefixes_changes_namespace() {
        let opts = super::ListRefsOptions {
            prefixes: vec!["refs/changes/*".to_owned()],
            ..Default::default()
        };
        assert_eq!(
            super::list_ref_prefixes(&opts),
            vec!["refs/changes/".to_owned()]
        );
    }

    #[test]
    fn list_ref_prefixes_unfiltered_uses_refs_root() {
        let opts = super::ListRefsOptions::default();
        assert_eq!(super::list_ref_prefixes(&opts), vec!["refs/".to_owned()]);
    }

    #[test]
    fn patterned_peel_line_excluded_for_short_tag_pattern() {
        let opts = super::ListRefsOptions {
            prefixes: vec!["v1".to_owned()],
            peel: true,
            ..Default::default()
        };
        assert!(super::ref_matches_list_opts("refs/tags/v1", &opts));
        assert!(!super::ref_matches_list_opts("refs/tags/v1^{}", &opts));
    }
}
