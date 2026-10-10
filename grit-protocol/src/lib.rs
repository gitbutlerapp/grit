//! Git smart protocol handlers for HTTP transport.
//!
//! Runs upload-pack and receive-pack in-process via [`grit_lib::serve`], with
//! no subprocess or external `grit` binary.

pub mod receive_pack;
pub mod upload_pack;

use std::path::{Path, PathBuf};

use grit_lib::config::ConfigSet;
use grit_lib::environment::Environment;

pub use grit_lib::environment::RepositoryOptions;
use grit_lib::repo::Repository;
use grit_lib::serve::{self, ProtocolVersion, ReceivePolicy, ServeOptions};

/// Errors from smart-protocol handlers.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The path does not look like a git repository.
    #[error("not a git repository: {0}")]
    NotARepository(String),
    /// Serving the wire protocol failed.
    #[error(transparent)]
    Serve(#[from] serve::ServeError),
    /// Opening or reading repository state failed.
    #[error(transparent)]
    Repository(#[from] grit_lib::error::Error),
    /// Buffered I/O failed while collecting the response.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Result alias for protocol handlers.
pub type Result<T> = std::result::Result<T, Error>;

/// Build [`RepositoryOptions`] from the current process environment.
///
/// Captures `HOME`, `GIT_CONFIG_*`, and the other variables [`ConfigSet::load`]
/// uses for global and system config. Call once at server startup and pass the
/// same options into every handler so receive policy and hide-ref rules match
/// a `grit receive-pack` / `grit upload-pack` subprocess.
#[must_use]
pub fn repository_options_from_process() -> RepositoryOptions {
    RepositoryOptions::with_environment(Environment::capture_process())
}

/// Map an explicit smart-HTTP protocol version header to a wire version.
#[must_use]
pub fn protocol_version_from_header(version: Option<u8>) -> ProtocolVersion {
    match version {
        Some(2) => ProtocolVersion::V2,
        Some(1) => ProtocolVersion::V1,
        _ => ProtocolVersion::V0,
    }
}

/// Validate that a path looks like a git repository (bare or non-bare).
///
/// # Errors
///
/// Returns [`Error::NotARepository`] when neither `HEAD` nor `.git/HEAD` exists.
pub fn validate_repo_path(path: &Path) -> Result<PathBuf> {
    if path.join("HEAD").is_file() {
        return Ok(path.to_path_buf());
    }
    let dot_git = path.join(".git");
    if dot_git.join("HEAD").is_file() {
        return Ok(path.to_path_buf());
    }
    Err(Error::NotARepository(path.display().to_string()))
}

pub(crate) fn run_upload_pack(
    repo_path: &Path,
    protocol_version: Option<u8>,
    stateless_rpc: bool,
    advertise_refs: bool,
    input: &[u8],
    repository_options: &RepositoryOptions,
) -> Result<Vec<u8>> {
    let repo = Repository::open_for_serving(repo_path, repository_options)?;
    let config = ConfigSet::load(repo.environment(), Some(&repo.git_dir), true).unwrap_or_default();
    let hidden_refs = grit_lib::hide_refs::hide_ref_patterns_uploadpack(&config);
    let opts = ServeOptions {
        protocol: protocol_version_from_header(protocol_version),
        stateless_rpc,
        advertise_refs,
        agent: format!("grit/{}", env!("CARGO_PKG_VERSION")),
        hidden_refs,
    };
    let mut output = Vec::new();
    serve::upload_pack(&repo, &mut &*input, &mut output, &opts)?;
    Ok(output)
}

pub(crate) fn run_receive_pack(
    repo_path: &Path,
    stateless_rpc: bool,
    advertise_refs: bool,
    input: &[u8],
    repository_options: &RepositoryOptions,
) -> Result<Vec<u8>> {
    let repo = Repository::open_for_serving(repo_path, repository_options)?;
    let config = ConfigSet::load(repo.environment(), Some(&repo.git_dir), true).unwrap_or_default();
    let hidden_refs = grit_lib::hide_refs::hide_ref_patterns_receive(&config);
    let opts = ServeOptions {
        protocol: ProtocolVersion::V0,
        stateless_rpc,
        advertise_refs,
        agent: format!("grit/{}", env!("CARGO_PKG_VERSION")),
        hidden_refs,
    };
    let policy = ReceivePolicy::from_config(&config);
    let mut output = Vec::new();
    serve::receive_pack(&repo, &mut &*input, &mut output, &opts, &policy)?;
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use grit_lib::serve::ProtocolVersion;

    #[test]
    fn protocol_header_maps_versions() {
        assert_eq!(protocol_version_from_header(None), ProtocolVersion::V0);
        assert_eq!(protocol_version_from_header(Some(2)), ProtocolVersion::V2);
    }
}
