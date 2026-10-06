//! Default-remote discovery, authentication selection, and transport dispatch
//! shared by the `gritx-fetch` and `gritx-push` examples.
//!
//! The interesting part — and the reason these two examples exist — is that the
//! authentication a Git remote needs is implied entirely by its URL scheme:
//!
//! * `http(s)://` — smart HTTP. Credentials come from the configured
//!   `credential.helper` programs (e.g. `osxkeychain`, `cache`, `store`), filled
//!   on a `401` and retried as HTTP Basic. We wire grit-lib's
//!   [`HelperCredentialProvider`] into the HTTP client so this happens
//!   automatically (and fails with a typed error, never a TTY prompt).
//! * `ssh://` / `git@host:path` — SSH. Authentication is SSH's own job (keys or
//!   an agent), handled by the `ssh` child process; grit-lib does nothing.
//! * `git://` — the anonymous Git daemon protocol. No authentication.
//! * `file://` / a local path — a local repository. No authentication.
//!
//! [`resolve_remote`] picks the default remote and classifies its URL;
//! [`describe_auth`] renders the human "here's the auth I'll use" line; and the
//! [`fetch`]/[`push`] helpers run the operation over the matching transport.

use std::path::Path;
use std::path::PathBuf;

use anyhow::Context as _;
use anyhow::Result;
use grit_lib::config::ConfigSet;
use grit_lib::credentials::HelperCredentialProvider;
use grit_lib::fetch::fetch_remote;
use grit_lib::fetch::NoProgress;
use grit_lib::push::push_http;
use grit_lib::push::push_remote;
use grit_lib::transfer;
use grit_lib::transfer::FetchOptions;
use grit_lib::transfer::FetchOutcome;
use grit_lib::transfer::PushOptions;
use grit_lib::transfer::PushOutcome;
use grit_lib::transfer::PushRefSpec;
use grit_lib::transport::http::http_fetch;
use grit_lib::transport::http::ureq_client::UreqHttpClient;
use grit_lib::transport::ConnectOptions;
use grit_lib::transport::GitDaemonTransport;
use grit_lib::transport::Service;
use grit_lib::transport::SshTransport;
use grit_lib::transport::Transport as _;

/// How a remote URL is reached, and therefore what authentication it implies.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RemoteKind {
    /// `http(s)://` — smart HTTP; auth via configured credential helpers.
    Http,
    /// `git://` — anonymous Git daemon; no auth.
    GitDaemon,
    /// `ssh://`, `git+ssh://`, or scp-style `host:path`; auth via SSH keys/agent.
    Ssh,
    /// `file://` or a local filesystem path; no auth.
    Local,
}

impl RemoteKind {
    /// Classify a remote URL by scheme.
    pub fn classify(url: &str) -> Self {
        if url.starts_with("http://") || url.starts_with("https://") {
            Self::Http
        } else if url.starts_with("git://") {
            Self::GitDaemon
        } else if grit_lib::transport::is_ssh_url(url) {
            Self::Ssh
        } else {
            // `file://...` or a bare path.
            Self::Local
        }
    }

    /// A short transport label for display.
    pub fn label(self) -> &'static str {
        match self {
            Self::Http => "smart HTTP",
            Self::GitDaemon => "git:// (anonymous daemon)",
            Self::Ssh => "SSH",
            Self::Local => "local (file)",
        }
    }
}

/// A resolved remote: its name, the URL to use, the transport kind, and the
/// fetch refspecs configured for it.
pub struct Remote {
    pub name: String,
    pub url: String,
    pub kind: RemoteKind,
    pub fetch_refspecs: Vec<String>,
}

/// Pick the remote name: an explicit argument, else the current branch's
/// `branch.<name>.remote`, else `origin` (matching Git's default selection).
fn default_remote_name(config: &ConfigSet, git_dir: &Path, explicit: Option<&str>) -> String {
    if let Some(name) = explicit {
        return name.to_owned();
    }
    if let Ok(Some(head)) = grit_lib::refs::read_symbolic_ref(git_dir, "HEAD") {
        if let Some(branch) = head.strip_prefix("refs/heads/") {
            if let Some(remote) = config.get(&format!("branch.{branch}.remote")) {
                if !remote.trim().is_empty() {
                    return remote;
                }
            }
        }
    }
    "origin".to_owned()
}

/// Resolve the remote to operate on. `for_push` selects `remote.<name>.pushurl`
/// when present (Git prefers it for pushing), otherwise `remote.<name>.url`.
pub fn resolve_remote(
    config: &ConfigSet,
    git_dir: &Path,
    explicit: Option<&str>,
    for_push: bool,
) -> Result<Remote> {
    let name = default_remote_name(config, git_dir, explicit);
    let url = for_push
        .then(|| config.get(&format!("remote.{name}.pushurl")))
        .flatten()
        .or_else(|| config.get(&format!("remote.{name}.url")))
        .filter(|u| !u.trim().is_empty())
        .with_context(|| {
            format!("remote '{name}' has no configured URL (set remote.{name}.url)")
        })?;
    let mut fetch_refspecs = config.get_all(&format!("remote.{name}.fetch"));
    fetch_refspecs.retain(|s| !s.trim().is_empty());
    if fetch_refspecs.is_empty() {
        fetch_refspecs = vec![format!("+refs/heads/*:refs/remotes/{name}/*")];
    }
    let kind = RemoteKind::classify(&url);
    Ok(Remote {
        name,
        url,
        kind,
        fetch_refspecs,
    })
}

/// Human-readable description of the authentication that will be used — the
/// "discovery" the examples are meant to show.
pub fn describe_auth(config: &ConfigSet, remote: &Remote) -> String {
    match remote.kind {
        RemoteKind::Http => {
            let helpers = http_credential_helpers(config, &remote.url);
            if helpers.is_empty() {
                "HTTP Basic — no credential.helper configured (a 401 will fail with \
                 a typed auth error, never a prompt)"
                    .to_owned()
            } else {
                format!(
                    "HTTP Basic, filled on 401 by credential helper(s): {}",
                    helpers.join(", ")
                )
            }
        }
        RemoteKind::Ssh => format!("SSH keys/agent via `{}`", ssh_command()),
        RemoteKind::GitDaemon => "none (anonymous git:// protocol)".to_owned(),
        RemoteKind::Local => "none (local repository)".to_owned(),
    }
}

/// The `credential.helper` values that apply to `url` — the section-default
/// `credential.helper` (applies to every URL) plus any URL-scoped
/// `credential.<pattern>.helper` matched with Git's urlmatch rules. This mirrors
/// what [`HelperCredentialProvider`] will actually consult.
fn http_credential_helpers(config: &ConfigSet, url: &str) -> Vec<String> {
    let mut helpers: Vec<String> = Vec::new();
    let push = |val: &str, helpers: &mut Vec<String>| {
        let val = val.trim().to_owned();
        if !val.is_empty() && !helpers.contains(&val) {
            helpers.push(val);
        }
    };
    // Section-default `credential.helper` (urlmatch only reports URL-scoped keys).
    for val in config.get_all("credential.helper") {
        push(&val, &mut helpers);
    }
    // URL-scoped `credential.<pattern>.helper`.
    for (var, val, _scope) in
        grit_lib::config::get_urlmatch_all_in_section(config.entries(), "credential", url)
    {
        if var.eq_ignore_ascii_case("helper") {
            push(&val, &mut helpers);
        }
    }
    helpers
}

/// The SSH command an `ssh` remote would use, for display only.
fn ssh_command() -> String {
    std::env::var("GIT_SSH_COMMAND")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .or_else(|| {
            std::env::var("GIT_SSH")
                .ok()
                .filter(|v| !v.trim().is_empty())
        })
        .unwrap_or_else(|| "ssh".to_owned())
}

fn local_git_dir(url: &str, git_dir: &Path) -> PathBuf {
    grit_lib::transport_path::resolve_local_remote_git_dir(url, git_dir)
}

/// Build an HTTP client honoring the repo's request-shaping config
/// (`http.proxy`, `http.cookieFile`/`saveCookies`, `http.extraHeader`) and wired
/// with a config-driven [`HelperCredentialProvider`] so `credential.helper`
/// programs satisfy a `401`. Falls back to a plain client if config can't load.
fn http_client(git_dir: &Path, git_protocol: Option<&str>) -> UreqHttpClient {
    let client = match ConfigSet::load(Some(git_dir), true) {
        Ok(config) => {
            let provider = HelperCredentialProvider::new(config.clone());
            match UreqHttpClient::from_config(&config) {
                Ok(c) => c.with_credential_provider(Box::new(provider)),
                Err(_) => UreqHttpClient::with_credentials(Box::new(provider)),
            }
        }
        Err(_) => UreqHttpClient::new(),
    };
    match git_protocol {
        Some(v) => client.with_git_protocol(v.to_owned()),
        None => client,
    }
}

/// Run a fetch from `remote` into `git_dir` over the matching transport,
/// requesting protocol v2 for the wire transports.
pub fn fetch(git_dir: &Path, remote: &Remote, opts: &FetchOptions) -> Result<FetchOutcome> {
    let v2 = ConnectOptions {
        protocol_version: 2,
        ..Default::default()
    };
    let outcome = match remote.kind {
        RemoteKind::Http => {
            let client = http_client(git_dir, Some("version=2"));
            http_fetch(&client, git_dir, &remote.url, opts, &mut NoProgress)?
        }
        RemoteKind::GitDaemon => {
            let mut conn =
                GitDaemonTransport::new().connect(&remote.url, Service::UploadPack, &v2)?;
            fetch_remote(git_dir, &mut *conn, opts, &mut NoProgress)?
        }
        RemoteKind::Ssh => {
            let mut conn = SshTransport::new().connect(&remote.url, Service::UploadPack, &v2)?;
            fetch_remote(git_dir, &mut *conn, opts, &mut NoProgress)?
        }
        RemoteKind::Local => {
            transfer::fetch_local(git_dir, &local_git_dir(&remote.url, git_dir), opts)?
        }
    };
    Ok(outcome)
}

/// Run a push of `refs` to `remote` from `git_dir` over the matching transport.
/// Push uses protocol v0/v1 (Git has no v2 receive-pack).
pub fn push(
    git_dir: &Path,
    remote: &Remote,
    refs: &[PushRefSpec],
    opts: &PushOptions,
) -> Result<PushOutcome> {
    let v0 = ConnectOptions::default();
    let outcome = match remote.kind {
        RemoteKind::Http => {
            // No `version=2` header: smart-HTTP receive-pack is v0/v1.
            let client = http_client(git_dir, None);
            push_http(&client, git_dir, &remote.url, refs, opts, &mut NoProgress)?
        }
        RemoteKind::GitDaemon => {
            let mut conn =
                GitDaemonTransport::new().connect(&remote.url, Service::ReceivePack, &v0)?;
            push_remote(git_dir, &mut *conn, refs, opts, &mut NoProgress)?
        }
        RemoteKind::Ssh => {
            let mut conn = SshTransport::new().connect(&remote.url, Service::ReceivePack, &v0)?;
            push_remote(git_dir, &mut *conn, refs, opts, &mut NoProgress)?
        }
        RemoteKind::Local => {
            transfer::push_local(git_dir, &local_git_dir(&remote.url, git_dir), refs, opts)?
        }
    };
    Ok(outcome)
}
