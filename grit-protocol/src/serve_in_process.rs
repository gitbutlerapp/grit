//! Run upload-pack / receive-pack in-process via [`grit_lib::serve`].
//!
//! Avoids spawning a `grit` subprocess for every smart-HTTP request (the dominant
//! cost in push/clone benchmarks against `git http-backend`).

use std::io::Cursor;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use grit_lib::config::ConfigSet;
use grit_lib::environment::{Environment, RepositoryOptions};
use grit_lib::repo::Repository;
use grit_lib::serve::{self, ProtocolVersion, ReceivePolicy, ServeOptions};

fn open_served_repo(path: &Path) -> Result<Repository> {
    let options = RepositoryOptions::with_environment(Environment::empty());
    let mut with_git_suffix = path.as_os_str().to_owned();
    with_git_suffix.push(".git");
    let candidates = [
        (path.join(".git"), Some(path.to_path_buf())),
        (path.to_path_buf(), None),
        (PathBuf::from(with_git_suffix), None),
    ];
    for (git_dir, work_tree) in candidates {
        if git_dir.join("HEAD").is_file() {
            return Repository::open_with_options(&git_dir, work_tree.as_deref(), options)
                .context("open served repository");
        }
    }
    anyhow::bail!("not a git repository: {}", path.display())
}

fn serve_options(
    config: &ConfigSet,
    service: ServiceKind,
    advertise_refs: bool,
    stateless_rpc: bool,
    protocol_version: Option<u8>,
) -> ServeOptions {
    let hidden_refs = match service {
        ServiceKind::UploadPack => grit_lib::hide_refs::hide_ref_patterns_uploadpack(config),
        ServiceKind::ReceivePack => grit_lib::hide_refs::hide_ref_patterns_receive(config),
    };
    let protocol = protocol_version
        .map(|v| match v {
            2 => ProtocolVersion::V2,
            1 => ProtocolVersion::V1,
            _ => ProtocolVersion::V0,
        })
        .unwrap_or(ProtocolVersion::V0);
    ServeOptions {
        protocol,
        stateless_rpc,
        advertise_refs,
        agent: format!("grit/{}", env!("CARGO_PKG_VERSION")),
        hidden_refs,
    }
}

#[derive(Copy, Clone)]
enum ServiceKind {
    UploadPack,
    ReceivePack,
}

/// In-process receive-pack (advertise or stateless RPC body).
pub fn receive_pack(repo_path: &Path, stdin: &[u8], advertise_refs: bool) -> Result<Vec<u8>> {
    let repo = open_served_repo(repo_path)?;
    let config = ConfigSet::load(repo.environment(), Some(&repo.git_dir), true).unwrap_or_default();
    let opts = serve_options(
        &config,
        ServiceKind::ReceivePack,
        advertise_refs,
        !advertise_refs,
        None,
    );
    let policy = ReceivePolicy::from_config(&config);
    let mut input = Cursor::new(stdin);
    let mut output = Vec::new();
    serve::receive_pack(&repo, &mut input, &mut output, &opts, &policy)
        .context("receive-pack serve")?;
    Ok(output)
}

/// In-process upload-pack (advertise or stateless RPC body).
pub fn upload_pack(
    repo_path: &Path,
    stdin: &[u8],
    advertise_refs: bool,
    protocol_version: Option<u8>,
) -> Result<Vec<u8>> {
    let repo = open_served_repo(repo_path)?;
    let config = ConfigSet::load(repo.environment(), Some(&repo.git_dir), true).unwrap_or_default();
    let opts = serve_options(
        &config,
        ServiceKind::UploadPack,
        advertise_refs,
        !advertise_refs,
        protocol_version,
    );
    let mut input = Cursor::new(stdin);
    let mut output = Vec::new();
    serve::upload_pack(&repo, &mut input, &mut output, &opts).context("upload-pack serve")?;
    Ok(output)
}
