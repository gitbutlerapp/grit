//! Upload-pack protocol handler (server side of fetch/clone).
//!
//! Serves fetch and clone over smart HTTP by calling [`grit_lib::remote::serve::upload_pack`]
//! in-process.

use std::io::Read;
use std::path::Path;

use grit_lib::config::environment::RepositoryOptions;

use crate::{run_upload_pack, run_upload_pack_to, Extensions, Result};

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
        &Extensions::default(),
    )
}

/// [`advertise_refs`] with [`Extensions`] (a `bundle-uri` bundle list).
///
/// # Errors
///
/// Same as [`advertise_refs`].
pub fn advertise_refs_with(
    repo_path: &Path,
    protocol_version: Option<u8>,
    repository_options: &RepositoryOptions,
    extensions: &Extensions,
) -> Result<Vec<u8>> {
    run_upload_pack(
        repo_path,
        protocol_version,
        false,
        true,
        &[],
        repository_options,
        extensions,
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
    stateless_rpc_from_reader(
        repo_path,
        &mut &*request_body,
        protocol_version,
        repository_options,
    )
}

/// Run a stateless upload-pack RPC exchange, reading the request body from a
/// stream.
///
/// Fetch and clone POST bodies can list one `want` line per ref, so the decoded
/// request may be much larger than the HTTP body when it is gzip-compressed.
/// The caller should apply its own decoded-size limit while reading; this
/// function consumes the body incrementally and never buffers it whole.
///
/// # Parameters
///
/// - `repo_path`: the repository to fetch from.
/// - `request_body`: the decoded request body (wants, haves, capabilities).
/// - `protocol_version`: the wire version from the `Git-Protocol` header.
/// - `repository_options`: environment used to load global/system config.
///
/// # Errors
///
/// Fails when the repository cannot be opened, the request cannot be read, or
/// the exchange fails before any response is produced.
pub fn stateless_rpc_from_reader(
    repo_path: &Path,
    request_body: &mut dyn Read,
    protocol_version: Option<u8>,
    repository_options: &RepositoryOptions,
) -> Result<Vec<u8>> {
    let mut output = Vec::new();
    stateless_rpc_to_reader(
        repo_path,
        request_body,
        protocol_version,
        repository_options,
        &Extensions::default(),
        &mut output,
    )?;
    Ok(output)
}

/// [`stateless_rpc`] with [`Extensions`], so `command=bundle-uri` is answered.
///
/// # Errors
///
/// Same as [`stateless_rpc`].
pub fn stateless_rpc_with(
    repo_path: &Path,
    request_body: &[u8],
    protocol_version: Option<u8>,
    repository_options: &RepositoryOptions,
    extensions: &Extensions,
) -> Result<Vec<u8>> {
    let mut output = Vec::new();
    stateless_rpc_to_reader(
        repo_path,
        &mut &*request_body,
        protocol_version,
        repository_options,
        extensions,
        &mut output,
    )?;
    Ok(output)
}

/// Run a stateless upload-pack RPC exchange (for `POST /git-upload-pack`),
/// writing the response to `output` as it is produced instead of collecting
/// it, so a server can stream a pack of any size in bounded memory.
///
/// # Parameters
///
/// - `repo_path`: the repository to fetch from.
/// - `request_body`: the decoded request body (wants, haves, capabilities).
/// - `protocol_version`: the wire version from the `Git-Protocol` header.
/// - `repository_options`: environment used to load global/system config.
/// - `extensions`: a `bundle-uri` bundle list to answer with.
/// - `output`: where the response goes.
///
/// # Errors
///
/// Fails when the repository cannot be opened or the request is rejected
/// before any output (for protocol v2 these go to `output` as an `ERR` line
/// instead), and with [`grit_lib::remote::serve::ServeError::PackInterrupted`]
/// when the pack fails after part of it was written: the caller must then end
/// the response as failed, not complete it.
pub fn stateless_rpc_to(
    repo_path: &Path,
    request_body: &[u8],
    protocol_version: Option<u8>,
    repository_options: &RepositoryOptions,
    extensions: &Extensions,
    output: &mut dyn std::io::Write,
) -> Result<()> {
    stateless_rpc_to_reader(
        repo_path,
        &mut &*request_body,
        protocol_version,
        repository_options,
        extensions,
        output,
    )
}

/// [`stateless_rpc_to`] with a streaming request body.
///
/// # Errors
///
/// Same as [`stateless_rpc_to`].
pub fn stateless_rpc_to_reader(
    repo_path: &Path,
    request_body: &mut dyn Read,
    protocol_version: Option<u8>,
    repository_options: &RepositoryOptions,
    extensions: &Extensions,
    output: &mut dyn std::io::Write,
) -> Result<()> {
    run_upload_pack_to(
        repo_path,
        protocol_version,
        true,
        false,
        request_body,
        repository_options,
        extensions,
        output,
    )
}
