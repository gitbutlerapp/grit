//! Receive-pack protocol handler (server side of push).
//!
//! Accepts pushes over smart HTTP by calling [`grit_lib::remote::serve::receive_pack`]
//! in-process.

use std::io::Read;
use std::path::Path;

use grit_lib::config::environment::RepositoryOptions;

use crate::{run_receive_pack, Result};

/// Run receive-pack ref advertisement (for `GET /info/refs?service=git-receive-pack`).
///
/// Returns the raw pkt-line advertisement bytes.
///
/// # Parameters
///
/// - `repository_options`: environment used to load global/system config (hide refs,
///   receive policy).
///
/// # Errors
///
/// Fails when `repo_path` is not a repository or advertisement generation fails.
pub fn advertise_refs(repo_path: &Path, repository_options: &RepositoryOptions) -> Result<Vec<u8>> {
    run_receive_pack(
        repo_path,
        false,
        true,
        &mut std::io::empty(),
        repository_options,
    )
}

/// Run a stateless receive-pack RPC exchange (for `POST /git-receive-pack`) on a
/// request body that is already in memory.
///
/// Takes the request body (pkt-line commands + pack data) and returns
/// the response (report-status). Prefer [`stateless_rpc_from_reader`] for HTTP
/// bodies, which can be as large as the pushed pack.
///
/// # Errors
///
/// Fails when the repository cannot be opened or the push is rejected.
pub fn stateless_rpc(
    repo_path: &Path,
    request_body: &[u8],
    repository_options: &RepositoryOptions,
) -> Result<Vec<u8>> {
    stateless_rpc_from_reader(repo_path, &mut &*request_body, repository_options)
}

/// Run a stateless receive-pack RPC exchange, reading the request body from a
/// stream.
///
/// The pushed pack is consumed from `request_body` as it arrives, so the caller
/// never has to hold the whole HTTP body in memory. The only size limit is the
/// repository's `receive.maxInputSize`, as with `git http-backend`.
///
/// # Parameters
///
/// - `repo_path`: the repository receiving the push.
/// - `request_body`: the decoded request body (pkt-line commands, then the pack).
/// - `repository_options`: environment used to load global/system config (hide refs,
///   receive policy).
///
/// # Returns
///
/// The response body (report-status, sideband-framed when the client asked for it).
///
/// # Errors
///
/// Fails when the repository cannot be opened, the command list is malformed, or
/// reading the request stream fails before the commands are parsed. A pack that
/// cannot be read or stored is reported to the client as an unpack error instead.
pub fn stateless_rpc_from_reader(
    repo_path: &Path,
    request_body: &mut dyn Read,
    repository_options: &RepositoryOptions,
) -> Result<Vec<u8>> {
    run_receive_pack(repo_path, true, false, request_body, repository_options)
}
