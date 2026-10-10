//! Receive-pack protocol handler (server side of push).
//!
//! Accepts pushes over smart HTTP by calling [`grit_lib::serve::receive_pack`]
//! in-process.

use std::path::Path;

use crate::{run_receive_pack, Result};

/// Run receive-pack ref advertisement (for `GET /info/refs?service=git-receive-pack`).
///
/// Returns the raw pkt-line advertisement bytes.
///
/// # Errors
///
/// Fails when `repo_path` is not a repository or advertisement generation fails.
pub fn advertise_refs(repo_path: &Path) -> Result<Vec<u8>> {
    run_receive_pack(repo_path, false, true, &[])
}

/// Run a stateless receive-pack RPC exchange (for `POST /git-receive-pack`).
///
/// Takes the request body (pkt-line commands + pack data) and returns
/// the response (report-status).
///
/// # Errors
///
/// Fails when the repository cannot be opened or the push is rejected.
pub fn stateless_rpc(repo_path: &Path, request_body: &[u8]) -> Result<Vec<u8>> {
    run_receive_pack(repo_path, true, false, request_body)
}
