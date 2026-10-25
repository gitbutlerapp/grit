//! Resumable pushes for `grit serve`: a TUS upload endpoint per repository.
//!
//! Receive-pack advertises `pushBundle=/push`, so a client with a large push
//! uploads it as a bundle to `/<repo>/push` with the [TUS] protocol (core plus
//! the creation extension) instead of one long `git-receive-pack` request:
//!
//! | Request | Answer |
//! | --- | --- |
//! | `OPTIONS /<repo>/push` | the TUS versions and extensions supported |
//! | `POST /<repo>/push` with `Upload-Length` | `201`, `Location: /<repo>/push/<id>` |
//! | `HEAD /<repo>/push/<id>` | `Upload-Offset`: bytes received so far |
//! | `PATCH /<repo>/push/<id>` with `Upload-Offset` | appends, answers the new offset |
//! | `POST /<repo>/push/<id>/receive` | applies it (see below) |
//!
//! Every request but `OPTIONS` needs write access, and an upload belongs to
//! the user who created it. Bytes are written to disk as they arrive, so a
//! dropped `PATCH` keeps what reached the server and the client continues from
//! the offset `HEAD` reports.
//!
//! Once every byte is in, the client posts the receive-pack command list to
//! `…/receive`. The server feeds that command list and the bundle's pack to
//! receive-pack, exactly as if they had arrived in one `git-receive-pack`
//! request, and answers with receive-pack's report. The upload is then
//! deleted. Uploads nobody finishes are deleted after [`STALE_AFTER`].
//!
//! [TUS]: https://tus.io/protocols/resumable-upload

use std::collections::HashSet;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use axum::body::Body;
use axum::http::{header, HeaderMap, Response, StatusCode};
use axum::response::IntoResponse;
use futures_util::StreamExt as _;
use grit_lib::remote::resumable::{TUS_PATCH_CONTENT_TYPE, TUS_VERSION};
use grit_protocol::RepositoryOptions;
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt as _;

/// What receive-pack advertises as `pushBundle`.
pub const PUSH_BUNDLE_PATH: &str = "/push";

/// URL path segment of the upload endpoint: `/<repo>/push`.
pub const PUSH_PATH: &str = "push";

/// Largest upload accepted.
pub const MAX_UPLOAD: u64 = 64 * 1024 * 1024 * 1024;

/// Largest command list accepted by `…/receive`.
const MAX_COMMANDS: usize = 4 * 1024 * 1024;

/// Uploads untouched for this long are deleted.
pub const STALE_AFTER: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// Which upload request a path names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UploadPath<'a> {
    /// `/<repo>/push`: create uploads.
    Endpoint,
    /// `/<repo>/push/<id>`: one upload.
    Upload(&'a str),
    /// `/<repo>/push/<id>/receive`: apply an upload.
    Receive(&'a str),
}

/// Split a request path into the repository name and the upload request, when
/// it is one.
#[must_use]
pub fn parse_path(path: &str) -> Option<(&str, UploadPath<'_>)> {
    let path = path.trim_end_matches('/');
    if let Some(name) = path.strip_suffix(&format!("/{PUSH_PATH}")) {
        return Some((name, UploadPath::Endpoint));
    }
    let (rest, last) = path.rsplit_once('/')?;
    if last == grit_lib::remote::push_bundle::RECEIVE_SUFFIX {
        let (rest, id) = rest.rsplit_once('/')?;
        let name = rest.strip_suffix(&format!("/{PUSH_PATH}"))?;
        return valid_id(id).then_some((name, UploadPath::Receive(id)));
    }
    let name = rest.strip_suffix(&format!("/{PUSH_PATH}"))?;
    valid_id(last).then_some((name, UploadPath::Upload(last)))
}

/// Upload ids are 32 lowercase hex digits the server made.
fn valid_id(id: &str) -> bool {
    id.len() == 32
        && id
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

/// What is stored next to an upload's bytes.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct UploadMeta {
    /// Total bytes the client said it would send.
    length: u64,
    /// The user who created the upload.
    user: i64,
}

/// The uploads in progress, so two requests never append to one at once.
#[derive(Debug, Default)]
pub struct UploadLocks {
    busy: Mutex<HashSet<PathBuf>>,
}

struct Busy<'a> {
    locks: &'a UploadLocks,
    path: PathBuf,
}

impl Drop for Busy<'_> {
    fn drop(&mut self) {
        self.locks
            .busy
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.path);
    }
}

impl UploadLocks {
    fn take(&self, path: &Path) -> Option<Busy<'_>> {
        let mut busy = self.busy.lock().unwrap_or_else(|e| e.into_inner());
        busy.insert(path.to_path_buf()).then(|| Busy {
            locks: self,
            path: path.to_path_buf(),
        })
    }
}

/// Where a repository keeps uploads.
fn upload_dir(repo_path: &Path) -> PathBuf {
    let dot_git = repo_path.join(".git");
    let git_dir = if dot_git.join("HEAD").is_file() {
        dot_git
    } else {
        repo_path.to_path_buf()
    };
    git_dir.join("grit-serve").join("uploads")
}

fn part_path(repo_path: &Path, id: &str) -> PathBuf {
    upload_dir(repo_path).join(format!("{id}.part"))
}

fn meta_path(repo_path: &Path, id: &str) -> PathBuf {
    upload_dir(repo_path).join(format!("{id}.json"))
}

/// The upload `id` if `user` owns it.
fn load(repo_path: &Path, id: &str, user: i64) -> Option<UploadMeta> {
    let meta: UploadMeta =
        serde_json::from_slice(&std::fs::read(meta_path(repo_path, id)).ok()?).ok()?;
    (meta.user == user).then_some(meta)
}

fn received(repo_path: &Path, id: &str) -> u64 {
    std::fs::metadata(part_path(repo_path, id))
        .map(|m| m.len())
        .unwrap_or(0)
}

fn remove(repo_path: &Path, id: &str) {
    let _ = std::fs::remove_file(part_path(repo_path, id));
    let _ = std::fs::remove_file(meta_path(repo_path, id));
}

/// Delete uploads nobody has touched for [`STALE_AFTER`].
fn sweep(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let stale = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age > STALE_AFTER);
        if stale {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

fn tus(status: StatusCode) -> axum::http::response::Builder {
    Response::builder()
        .status(status)
        .header("Tus-Resumable", TUS_VERSION)
        .header(header::CACHE_CONTROL, "no-store")
}

fn finish(builder: axum::http::response::Builder, body: Body) -> Response<Body> {
    builder
        .body(body)
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

fn status(code: StatusCode, message: &str) -> Response<Body> {
    finish(tus(code), Body::from(format!("{message}\n")))
}

fn header_u64(headers: &HeaderMap, name: &str) -> Option<u64> {
    headers.get(name)?.to_str().ok()?.trim().parse().ok()
}

/// `OPTIONS /<repo>/push`: what this endpoint supports.
#[must_use]
pub fn options() -> Response<Body> {
    finish(
        tus(StatusCode::NO_CONTENT)
            .header("Tus-Version", TUS_VERSION)
            .header("Tus-Extension", "creation")
            .header("Tus-Max-Size", MAX_UPLOAD.to_string()),
        Body::empty(),
    )
}

/// `POST /<repo>/push`: start an upload for `user`.
pub async fn create(
    repo_path: &Path,
    name: &str,
    user: i64,
    headers: &HeaderMap,
) -> Response<Body> {
    let Some(length) = header_u64(headers, "upload-length") else {
        return status(StatusCode::BAD_REQUEST, "Upload-Length is required");
    };
    if length == 0 || length > MAX_UPLOAD {
        return status(StatusCode::PAYLOAD_TOO_LARGE, "the upload is too large");
    }
    let dir = upload_dir(repo_path);
    if let Err(e) = std::fs::create_dir_all(&dir) {
        return status(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string());
    }
    sweep(&dir);
    let mut random = [0u8; 16];
    if let Err(e) = getrandom::getrandom(&mut random) {
        return status(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string());
    }
    let id = hex::encode(random);
    let meta = UploadMeta { length, user };
    let written = serde_json::to_vec(&meta)
        .map_err(|e| std::io::Error::other(e.to_string()))
        .and_then(|bytes| std::fs::write(meta_path(repo_path, &id), bytes))
        .and_then(|()| std::fs::File::create(part_path(repo_path, &id)).map(drop));
    if let Err(e) = written {
        remove(repo_path, &id);
        return status(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string());
    }
    finish(
        tus(StatusCode::CREATED)
            .header(header::LOCATION, format!("/{name}/{PUSH_PATH}/{id}"))
            .header("Upload-Offset", "0"),
        Body::empty(),
    )
}

/// `HEAD /<repo>/push/<id>`: how much has arrived.
#[must_use]
pub fn head(repo_path: &Path, id: &str, user: i64) -> Response<Body> {
    let Some(meta) = load(repo_path, id, user) else {
        return status(StatusCode::NOT_FOUND, "no such upload");
    };
    finish(
        tus(StatusCode::OK)
            .header("Upload-Offset", received(repo_path, id).to_string())
            .header("Upload-Length", meta.length.to_string()),
        Body::empty(),
    )
}

/// `PATCH /<repo>/push/<id>`: append the body at `Upload-Offset`.
pub async fn patch(
    repo_path: &Path,
    id: &str,
    user: i64,
    locks: &UploadLocks,
    headers: &HeaderMap,
    body: Body,
) -> Response<Body> {
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok());
    if content_type != Some(TUS_PATCH_CONTENT_TYPE) {
        return status(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "PATCH bodies must be application/offset+octet-stream",
        );
    }
    let Some(meta) = load(repo_path, id, user) else {
        return status(StatusCode::NOT_FOUND, "no such upload");
    };
    let part = part_path(repo_path, id);
    let Some(_busy) = locks.take(&part) else {
        return status(
            StatusCode::CONFLICT,
            "the upload is receiving another request",
        );
    };
    let mut offset = received(repo_path, id);
    if header_u64(headers, "upload-offset") != Some(offset) {
        return status(StatusCode::CONFLICT, "Upload-Offset does not match");
    }
    let mut file = match tokio::fs::OpenOptions::new().append(true).open(&part).await {
        Ok(f) => f,
        Err(e) => return status(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };
    let mut stream = body.into_data_stream();
    while let Some(chunk) = stream.next().await {
        // A dropped connection keeps what arrived; the client resumes from it.
        let Ok(chunk) = chunk else {
            break;
        };
        if offset + chunk.len() as u64 > meta.length {
            let _ = file.flush().await;
            return status(
                StatusCode::PAYLOAD_TOO_LARGE,
                "more bytes than Upload-Length",
            );
        }
        if let Err(e) = file.write_all(&chunk).await {
            return status(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string());
        }
        offset += chunk.len() as u64;
    }
    if let Err(e) = file.flush().await {
        return status(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string());
    }
    finish(
        tus(StatusCode::NO_CONTENT).header("Upload-Offset", offset.to_string()),
        Body::empty(),
    )
}

/// `POST /<repo>/push/<id>/receive`: apply a finished upload.
///
/// `body` is the receive-pack command list; the answer is receive-pack's
/// report, as for `POST /<repo>/git-receive-pack`. `on_applied` runs after a
/// push that receive-pack accepted (the server checks its bundle then).
#[allow(clippy::too_many_arguments)]
pub async fn receive(
    repo_path: PathBuf,
    id: &str,
    user: i64,
    locks: &UploadLocks,
    body: Body,
    options: RepositoryOptions,
    on_applied: impl FnOnce(),
) -> Response<Body> {
    let Some(meta) = load(&repo_path, id, user) else {
        return status(StatusCode::NOT_FOUND, "no such upload");
    };
    let part = part_path(&repo_path, id);
    let Some(_busy) = locks.take(&part) else {
        return status(
            StatusCode::CONFLICT,
            "the upload is receiving another request",
        );
    };
    if received(&repo_path, id) != meta.length {
        return status(StatusCode::CONFLICT, "the upload is not complete");
    }
    let commands = match axum::body::to_bytes(body, MAX_COMMANDS).await {
        Ok(b) => b,
        Err(_) => {
            return status(
                StatusCode::PAYLOAD_TOO_LARGE,
                "the command list is too large",
            )
        }
    };
    let applied = tokio::task::spawn_blocking({
        let repo_path = repo_path.clone();
        move || -> Result<Vec<u8>, String> {
            let bundle = grit_lib::bundle::Bundle::open(&part).map_err(|e| e.to_string())?;
            let mut pack = std::fs::File::open(&part).map_err(|e| e.to_string())?;
            pack.seek(SeekFrom::Start(bundle.pack_byte_offset()))
                .map_err(|e| e.to_string())?;
            let mut input = std::io::Cursor::new(commands).chain(pack);
            grit_protocol::receive_pack::stateless_rpc_from_reader(&repo_path, &mut input, &options)
                .map_err(|e| e.to_string())
        }
    })
    .await;
    let report = match applied {
        Ok(Ok(report)) => report,
        Ok(Err(e)) => return status(StatusCode::UNPROCESSABLE_ENTITY, &e),
        Err(e) => return status(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };
    remove(&repo_path, id);
    on_applied();
    finish(
        tus(StatusCode::OK).header(
            header::CONTENT_TYPE,
            "application/x-git-receive-pack-result",
        ),
        Body::from(report),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upload_paths_parse() {
        let id = "0123456789abcdef0123456789abcdef";
        assert_eq!(
            parse_path("repo/push"),
            Some(("repo", UploadPath::Endpoint))
        );
        assert_eq!(
            parse_path("repo.git/push/"),
            Some(("repo.git", UploadPath::Endpoint))
        );
        assert_eq!(
            parse_path(&format!("repo/push/{id}")),
            Some(("repo", UploadPath::Upload(id)))
        );
        assert_eq!(
            parse_path(&format!("repo/push/{id}/receive")),
            Some(("repo", UploadPath::Receive(id)))
        );
        assert_eq!(parse_path("repo/push/../x"), None);
        assert_eq!(parse_path("repo/push/ABC"), None);
        assert_eq!(parse_path("repo/info/refs"), None);
    }

    #[test]
    fn one_request_per_upload_at_a_time() {
        let locks = UploadLocks::default();
        let path = Path::new("/u/1.part");
        let first = locks.take(path);
        assert!(first.is_some());
        assert!(locks.take(path).is_none());
        drop(first);
        assert!(locks.take(path).is_some());
    }
}
