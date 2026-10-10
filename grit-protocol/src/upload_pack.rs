//! Upload-pack protocol handler (server side of fetch/clone).
//!
//! Serves fetch and clone over smart HTTP by calling [`grit_lib::serve::upload_pack`]
//! in-process.

use std::path::Path;

use grit_lib::environment::RepositoryOptions;

use crate::{run_upload_pack, Result};

/// Run upload-pack ref advertisement (for `GET /info/refs?service=git-upload-pack`).
///
/// Returns the raw pkt-line advertisement bytes (the v0/v1 ref list, or the v2
/// capability list when `protocol_version` is `Some(2)`) suitable for wrapping in an
/// HTTP response with the service header.
///
/// # Parameters
///
/// - `repository_options`: environment used to load global/system config (hide refs).
///
/// # Errors
///
/// Fails when `repo_path` is not a repository or advertisement generation fails.
pub fn advertise_refs(
    repo_path: &Path,
    protocol_version: Option<u8>,
    repository_options: &RepositoryOptions,
) -> Result<Vec<u8>> {
    run_upload_pack(
        repo_path,
        protocol_version,
        false,
        true,
        &[],
        repository_options,
    )
}

/// Run a stateless upload-pack RPC exchange (for `POST /git-upload-pack`).
///
/// Takes the request body as input and returns the response body.
/// Supports both protocol v0/v1 and v2 via `protocol_version`.
///
/// # Errors
///
/// Fails when the repository cannot be opened or the exchange fails.
pub fn stateless_rpc(
    repo_path: &Path,
    request_body: &[u8],
    protocol_version: Option<u8>,
    repository_options: &RepositoryOptions,
) -> Result<Vec<u8>> {
    run_upload_pack(
        repo_path,
        protocol_version,
        true,
        false,
        request_body,
        repository_options,
    )
}
