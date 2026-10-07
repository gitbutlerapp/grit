//! Upload-pack protocol handler (server side of fetch/clone).
//!
//! Wraps the `grit upload-pack` subprocess with piped I/O for use in
//! HTTP smart transport.

use anyhow::{Context, Result};
use std::path::Path;

/// Run upload-pack ref advertisement (for `GET /info/refs?service=git-upload-pack`).
///
/// Returns the raw pkt-line advertisement bytes (the v0/v1 ref list, or the v2
/// capability list when `protocol_version` is 2) suitable for wrapping in an
/// HTTP response with the service header.
pub fn advertise_refs(repo_path: &Path, protocol_version: Option<u8>) -> Result<Vec<u8>> {
    let output = crate::run_service(
        "upload-pack",
        &["--stateless-rpc", "--advertise-refs"],
        repo_path,
        protocol_version,
        &[],
    )
    .context("upload-pack advertisement failed")?;
    Ok(output)
}

/// Run a stateless upload-pack RPC exchange (for `POST /git-upload-pack`).
///
/// Takes the request body as input and returns the response body.
/// Supports both protocol v0/v1 and v2.
pub fn stateless_rpc(
    repo_path: &Path,
    request_body: &[u8],
    protocol_version: Option<u8>,
) -> Result<Vec<u8>> {
    let output = crate::run_service(
        "upload-pack",
        &["--stateless-rpc"],
        repo_path,
        protocol_version,
        request_body,
    )
    .context("upload-pack failed")?;
    Ok(output)
}
