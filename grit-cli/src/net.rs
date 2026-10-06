//! Remote transport glue: resolve a remote's URL and dispatch fetch/push to the
//! right grit-lib transport by URL scheme.
//!
//! The wire protocols, ref updates, and pack handling all live in grit-lib. The
//! only binary-specific piece is choosing a transport (and an HTTP client) for a
//! URL — grit-cli brings its own HTTP stack, so this dispatch lives here in
//! `grit` rather than in the shared library.

use anyhow::{bail, Context, Result};
use std::path::PathBuf;

use grit_lib::config::ConfigSet;
use grit_lib::credentials::HelperCredentialProvider;
use grit_lib::fetch::{fetch_remote, NoProgress};
use grit_lib::push::push_remote;
use grit_lib::repo::Repository;
use grit_lib::transfer::{
    fetch_local, push_local, FetchOptions, FetchOutcome, PushOptions, PushOutcome, PushRefSpec,
    TagMode,
};
use grit_lib::transport::http::ureq_client::UreqHttpClient;
use grit_lib::transport::http::{http_fetch, SmartHttpTransport};
use grit_lib::transport::{
    is_ssh_url, ConnectOptions, Connection, GitDaemonTransport, Service, SshTransport, Transport,
};

/// The remote `grit` uses when none is named or configured.
pub const DEFAULT_REMOTE: &str = "origin";

/// Look up `remote.<name>.url`.
pub fn remote_url(config: &ConfigSet, remote: &str) -> Result<String> {
    config
        .get(&format!("remote.{remote}.url"))
        .filter(|u| !u.trim().is_empty())
        .with_context(|| format!("remote '{remote}' has no configured URL"))
}

/// The fetch refspecs for a remote, defaulting to the standard tracking layout.
pub fn fetch_refspecs(config: &ConfigSet, remote: &str) -> Vec<String> {
    let configured = config.get_all(&format!("remote.{remote}.fetch"));
    if configured.is_empty() {
        vec![format!("+refs/heads/*:refs/remotes/{remote}/*")]
    } else {
        configured
    }
}

fn is_url_scheme(url: &str) -> bool {
    url.starts_with("http://")
        || url.starts_with("https://")
        || url.starts_with("git://")
        || is_ssh_url(url)
}

fn is_http(url: &str) -> bool {
    url.starts_with("http://") || url.starts_with("https://")
}

/// Build an HTTP client honoring the repo's request-shaping config and wired
/// with a `credential.helper`-backed provider, so a `401` is satisfied from the
/// credential store (e.g. a token saved by `grit auth`). Falls back to a plain
/// credentialed client if the config-driven build fails.
fn http_client(config: &ConfigSet) -> Result<UreqHttpClient> {
    let provider = Box::new(HelperCredentialProvider::new(config.clone()));
    let client = UreqHttpClient::from_config(config)
        .context("could not set up HTTP client")?
        .with_credential_provider(provider);
    Ok(client)
}

/// Resolve the git directory of a local remote (a path or `file://` URL).
fn local_git_dir(url: &str) -> PathBuf {
    let path = url.strip_prefix("file://").unwrap_or(url);
    let path = PathBuf::from(path);
    let dot_git = path.join(".git");
    if dot_git.is_dir() {
        dot_git
    } else {
        path
    }
}

fn connect(url: &str, service: Service) -> Result<Box<dyn Connection>> {
    let opts = ConnectOptions::default();
    let conn = if url.starts_with("git://") {
        GitDaemonTransport::new().connect(url, service, &opts)?
    } else if is_ssh_url(url) {
        SshTransport::new().connect(url, service, &opts)?
    } else {
        bail!("unsupported remote URL: {url}");
    };
    Ok(conn)
}

/// Fetch `refspecs` from `remote`, updating remote-tracking refs.
pub fn fetch(
    repo: &Repository,
    config: &ConfigSet,
    remote: &str,
    refspecs: Vec<String>,
) -> Result<FetchOutcome> {
    let url = remote_url(config, remote)?;
    let opts = FetchOptions {
        refspecs,
        tags: TagMode::Following,
        ..Default::default()
    };

    let outcome = if !is_url_scheme(&url) {
        fetch_local(&repo.git_dir, &local_git_dir(&url), &opts)?
    } else if is_http(&url) {
        let client = http_client(config)?;
        http_fetch(&client, &repo.git_dir, &url, &opts, &mut NoProgress)?
    } else {
        let mut conn = connect(&url, Service::UploadPack)?;
        fetch_remote(&repo.git_dir, &mut *conn, &opts, &mut NoProgress)?
    };
    Ok(outcome)
}

/// Push `refs` to `remote`.
pub fn push(
    repo: &Repository,
    config: &ConfigSet,
    remote: &str,
    refs: &[PushRefSpec],
) -> Result<PushOutcome> {
    let url = remote_url(config, remote)?;
    let opts = PushOptions {
        tracking_remote: Some(remote.to_owned()),
        ..PushOptions::default()
    };

    let outcome = if !is_url_scheme(&url) {
        push_local(&repo.git_dir, &local_git_dir(&url), refs, &opts)?
    } else if is_http(&url) {
        let client = http_client(config)?;
        SmartHttpTransport::new(client).push(&repo.git_dir, &url, refs, &opts, &mut NoProgress)?
    } else {
        let mut conn = connect(&url, Service::ReceivePack)?;
        push_remote(&repo.git_dir, &mut *conn, refs, &opts, &mut NoProgress)?
    };
    Ok(outcome)
}
