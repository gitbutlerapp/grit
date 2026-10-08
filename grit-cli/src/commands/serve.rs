//! `grit upload-pack` and `grit receive-pack` — the server side of fetch and push.
//!
//! These are plumbing commands. You don't run them by hand: a Git client starts
//! them on the remote end of an ssh connection, and `grit-http-server` runs them
//! to answer smart HTTP requests. They speak the Git wire protocol on stdin and
//! stdout, so they produce no human or JSON output of their own.

use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use crate::stdio;
use grit_lib::config::ConfigSet;
use grit_lib::repo::Repository;
use grit_lib::serve::{self, ProtocolVersion, ReceivePolicy, ServeOptions};

/// Which server program to run.
#[derive(Debug, Clone, Copy)]
pub enum Service {
    /// Send objects to a fetching or cloning client.
    UploadPack,
    /// Accept objects and ref updates from a pushing client.
    ReceivePack,
}

/// Run a server program against the repository at `directory`.
///
/// # Parameters
///
/// - `service`: upload-pack or receive-pack.
/// - `directory`: the repository to serve (a bare repository, a working tree,
///   or the same path with `.git` appended).
/// - `stateless_rpc`: answer one request without advertising first (smart HTTP).
/// - `advertise_refs`: print the advertisement and exit (smart HTTP discovery).
///
/// # Errors
///
/// Fails when `directory` is not a repository or the client breaks the protocol.
pub fn run(
    service: Service,
    directory: &str,
    stateless_rpc: bool,
    advertise_refs: bool,
) -> Result<()> {
    let repo = open_served_repo(Path::new(directory))?;
    let config = ConfigSet::load(&crate::context::environment(), Some(&repo.git_dir), true)
        .unwrap_or_default();
    let hidden_refs = match service {
        Service::UploadPack => grit_lib::hide_refs::hide_ref_patterns_uploadpack(&config),
        Service::ReceivePack => grit_lib::hide_refs::hide_ref_patterns_receive(&config),
    };
    let protocol = std::env::var("GIT_PROTOCOL")
        .map(|v| ProtocolVersion::from_git_protocol(&v))
        .unwrap_or_default();
    let opts = ServeOptions {
        protocol,
        stateless_rpc,
        advertise_refs,
        agent: format!("grit/{}", env!("CARGO_PKG_VERSION")),
        hidden_refs,
    };

    let stdin = std::io::stdin();
    let mut input = stdin.lock();
    let mut output = BufWriter::new(std::io::stdout().lock());
    match service {
        Service::UploadPack => serve::upload_pack(&repo, &mut input, &mut output, &opts)?,
        Service::ReceivePack => {
            let policy = ReceivePolicy::from_config(&config);
            serve::receive_pack(&repo, &mut input, &mut output, &opts, &policy)?;
        }
    }
    stdio::io_result(output.flush()).context("writing the response")?;
    Ok(())
}

/// Open the repository a client asked for, without searching parent
/// directories: a server must only serve exactly the path it was given.
fn open_served_repo(path: &Path) -> Result<Repository> {
    let mut with_git_suffix = path.as_os_str().to_owned();
    with_git_suffix.push(".git");
    let candidates = [
        (path.join(".git"), Some(path.to_path_buf())),
        (path.to_path_buf(), None),
        (PathBuf::from(with_git_suffix), None),
    ];
    for (git_dir, work_tree) in candidates {
        if !git_dir.join("HEAD").is_file() {
            continue;
        }
        return Repository::open(&git_dir, work_tree.as_deref())
            .with_context(|| format!("opening repository '{}'", path.display()));
    }
    bail!(
        "'{}' does not appear to be a git repository",
        path.display()
    )
}
