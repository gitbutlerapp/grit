//! Receive-pack protocol handler (server side of push).
//!
//! Wraps the `grit receive-pack` subprocess with piped I/O for use in
//! HTTP smart transport.

use anyhow::{Context, Result};
use std::path::Path;

/// Run receive-pack ref advertisement (for `GET /info/refs?service=git-receive-pack`).
///
/// Returns the raw pkt-line advertisement bytes.
pub fn advertise_refs(repo_path: &Path) -> Result<Vec<u8>> {
    let output = crate::run_service(
        "receive-pack",
        &["--stateless-rpc", "--advertise-refs"],
        repo_path,
        None,
        &[],
    )
    .context("receive-pack advertisement failed")?;
    Ok(output)
}

/// Run a stateless receive-pack RPC exchange (for `POST /git-receive-pack`).
///
/// Takes the request body (pkt-line commands + pack data) and returns
/// the response (report-status).
pub fn stateless_rpc(repo_path: &Path, request_body: &[u8]) -> Result<Vec<u8>> {
    let output = crate::run_service(
        "receive-pack",
        &["--stateless-rpc"],
        repo_path,
        None,
        request_body,
    )
    .context("receive-pack failed")?;
    Ok(output)
}
