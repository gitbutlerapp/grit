//! Default-remote discovery, authentication selection, and transport dispatch
//! shared by the `gritx-fetch` and `gritx-push` examples.
//!
//! Operations go through [`grit_lib::remote::Remote`] — the same dispatcher the
//! `grit` CLI uses.

use std::path::Path;

use anyhow::Context as _;
use anyhow::Result;
use grit_lib::config::ConfigSet;
use grit_lib::fetch::NoProgress;
use grit_lib::remote::{DefaultHttpClientFactory, Remote, RemoteUrl};
use grit_lib::transfer::FetchOptions;
use grit_lib::transfer::FetchOutcome;
use grit_lib::transfer::PushOptions;
use grit_lib::transfer::PushOutcome;
use grit_lib::transfer::PushRefSpec;

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
        match RemoteUrl::try_from(url) {
            Ok(RemoteUrl::Http(_) | RemoteUrl::Https(_)) => Self::Http,
            Ok(RemoteUrl::Git(_)) => Self::GitDaemon,
            Ok(RemoteUrl::Ssh(_)) => Self::Ssh,
            Ok(RemoteUrl::Local(_) | RemoteUrl::File(_)) => Self::Local,
            Err(_) => Self::Local,
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
pub struct RemoteInfo {
    pub name: String,
    pub url: String,
    pub kind: RemoteKind,
    pub fetch_refspecs: Vec<String>,
    pub remote: Remote,
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

/// Resolve the remote to operate on.
pub fn resolve_remote(
    config: &ConfigSet,
    git_dir: &Path,
    explicit: Option<&str>,
    _for_push: bool,
) -> Result<RemoteInfo> {
    let name = default_remote_name(config, git_dir, explicit);
    let remote = Remote::from_config(config, &name)
        .with_context(|| format!("remote '{name}' has no configured URL"))?;
    let url = remote.fetch_url().to_url_string();
    let kind = RemoteKind::classify(&url);
    let fetch_refspecs = remote.fetch_refspecs.clone();
    Ok(RemoteInfo {
        name,
        url,
        kind,
        fetch_refspecs,
        remote,
    })
}

/// Human-readable description of the authentication that will be used — the
/// "discovery" the examples are meant to show.
pub fn describe_auth(config: &ConfigSet, remote: &RemoteInfo) -> String {
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

/// The `credential.helper` values that apply to `url`.
fn http_credential_helpers(config: &ConfigSet, url: &str) -> Vec<String> {
    let mut helpers: Vec<String> = Vec::new();
    let push = |val: &str, helpers: &mut Vec<String>| {
        let val = val.trim().to_owned();
        if !val.is_empty() && !helpers.contains(&val) {
            helpers.push(val);
        }
    };
    for val in config.get_all("credential.helper") {
        push(&val, &mut helpers);
    }
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

/// Run a fetch from `remote` into `repo` via [`Remote::fetch`].
pub fn fetch(
    repo: &grit_lib::repo::Repository,
    remote: &RemoteInfo,
    opts: &FetchOptions,
) -> Result<FetchOutcome> {
    let factory = DefaultHttpClientFactory;
    remote
        .remote
        .fetch(repo, opts.clone(), &mut NoProgress, Some(&factory))
        .map_err(|e| anyhow::Error::msg(e.to_string()))
}

/// Run a push of `refs` to `remote` from `repo` via [`Remote::push`].
pub fn push(
    repo: &grit_lib::repo::Repository,
    remote: &RemoteInfo,
    refs: &[PushRefSpec],
    opts: &PushOptions,
) -> Result<PushOutcome> {
    let factory = DefaultHttpClientFactory;
    remote
        .remote
        .push(repo, refs, opts.clone(), &mut NoProgress, Some(&factory))
        .map_err(|e| anyhow::Error::msg(e.to_string()))
}
