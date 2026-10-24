//! Minimal Git smart HTTP server powered by grit.
//!
//! Implements the four endpoints required for git clone/push over HTTP:
//!   GET  /:repo/info/refs?service=git-upload-pack
//!   GET  /:repo/info/refs?service=git-receive-pack
//!   POST /:repo/git-upload-pack
//!   POST /:repo/git-receive-pack

use std::io::Read;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use axum::{
    body::{Body, Bytes},
    extract::{Path, Query, Request, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use clap::Parser;

#[derive(Parser)]
#[command(
    name = "grit-http-server",
    about = "Git smart HTTP server powered by grit"
)]
struct Args {
    /// Root directory containing bare repositories
    #[arg(short, long, default_value = ".")]
    root: PathBuf,

    /// Address to bind to
    #[arg(short, long, default_value = "127.0.0.1:9418")]
    bind: String,

    /// Require HTTP basic auth: `user:pass`. When set, every request must carry a
    /// matching `Authorization: Basic <base64(user:pass)>` header; otherwise the
    /// server replies `401 Unauthorized` with `WWW-Authenticate: Basic realm="git"`.
    /// Used to exercise the credential / 401-retry path end-to-end.
    #[arg(long, value_name = "USER:PASS")]
    require_auth: Option<String>,

    /// Append every received request's headers to this file (one `name: value`
    /// per line, a blank line between requests). Lets a test assert that a
    /// configured `Cookie` / `http.extraHeader` actually reached the server.
    #[arg(long, value_name = "FILE")]
    log_headers: Option<PathBuf>,

    /// Emit this `Set-Cookie` header value on every response. Lets a test exercise
    /// the client's `http.saveCookies` persistence path end-to-end.
    #[arg(long, value_name = "NAME=VALUE")]
    set_cookie: Option<String>,
}

#[derive(Clone)]
struct AppState {
    root: PathBuf,
    /// Repository open/config options captured from the server process at startup.
    repository_options: grit_protocol::RepositoryOptions,
    /// The expected `Authorization: Basic …` header value when basic auth is
    /// required (`Some` iff `--require-auth` was given), precomputed once.
    expected_auth: Option<String>,
    /// Where to append received request headers (`--log-headers`), if set.
    log_headers: Option<PathBuf>,
    /// A `Set-Cookie` value to emit on every response (`--set-cookie`), if set.
    set_cookie: Option<String>,
}

impl AppState {
    /// Append a request's headers to the `--log-headers` file (if configured),
    /// one `name: value` line per header, terminated by a blank line. Best-effort:
    /// any I/O error is ignored so logging never affects the served response.
    fn log_request_headers(&self, headers: &HeaderMap) {
        let Some(path) = self.log_headers.as_ref() else {
            return;
        };
        use std::io::Write as _;
        let mut buf = String::new();
        for (name, value) in headers {
            let v = value.to_str().unwrap_or("<non-utf8>");
            buf.push_str(name.as_str());
            buf.push_str(": ");
            buf.push_str(v);
            buf.push('\n');
        }
        buf.push('\n');
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            let _ = f.write_all(buf.as_bytes());
        }
    }

    /// Apply the `--set-cookie` response header to `builder` if configured.
    fn apply_set_cookie(
        &self,
        builder: axum::http::response::Builder,
    ) -> axum::http::response::Builder {
        match self.set_cookie.as_deref() {
            Some(value) => builder.header(header::SET_COOKIE, value),
            None => builder,
        }
    }
}

impl AppState {
    /// `Ok(())` when the request is authorized (or auth is not required); `Err`
    /// is a ready-to-return `401` challenge response otherwise.
    fn check_auth(&self, headers: &HeaderMap) -> Result<(), Box<Response>> {
        let Some(expected) = &self.expected_auth else {
            return Ok(());
        };
        let presented = headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok());
        if presented == Some(expected.as_str()) {
            return Ok(());
        }
        Err(Box::new(unauthorized()))
    }
}

/// A `401 Unauthorized` response advertising HTTP basic auth, matching what a
/// real Git server returns so the client's `WWW-Authenticate` parsing + 401
/// retry path is exercised.
fn unauthorized() -> Response {
    axum::http::Response::builder()
        .status(StatusCode::UNAUTHORIZED)
        .header(header::WWW_AUTHENTICATE, "Basic realm=\"git\"")
        .body(axum::body::Body::from("authentication required"))
        .unwrap_or_else(|_| StatusCode::UNAUTHORIZED.into_response())
}

/// Build the expected `Authorization: Basic …` header value from a
/// `user:pass` string.
fn expected_basic_auth(user_pass: &str) -> String {
    use base64::Engine as _;
    let encoded = base64::engine::general_purpose::STANDARD.encode(user_pass.as_bytes());
    format!("Basic {encoded}")
}

impl AppState {
    fn repo_path(&self, repo: &str) -> Result<PathBuf, Box<Response>> {
        // Sanitize: reject path traversal
        if repo.contains("..") {
            return Err(Box::new(
                (StatusCode::BAD_REQUEST, "invalid repository path").into_response(),
            ));
        }
        let path = self.root.join(repo);
        grit_protocol::validate_repo_path(&path)
            .map_err(|_| Box::new((StatusCode::NOT_FOUND, "repository not found").into_response()))
    }
}

#[derive(serde::Deserialize)]
struct InfoRefsQuery {
    service: Option<String>,
}

fn pkt_line(data: &str) -> Vec<u8> {
    let len = data.len() + 4;
    format!("{len:04x}{data}").into_bytes()
}

/// Parse the requested protocol version from the `Git-Protocol` request header.
///
/// Git sends `Git-Protocol: version=2` to request protocol v2 over smart HTTP;
/// the header carries colon-separated key=value entries (e.g.
/// `version=2:agent=git/2`). We extract the `version=` value. Returns `None`
/// (the classic v0/v1 advertisement / stateless RPC) when absent or not v2.
fn protocol_version_from_headers(headers: &HeaderMap) -> Option<u8> {
    let raw = headers.get("Git-Protocol")?.to_str().ok()?;
    for entry in raw.split([':', ' ']) {
        if let Some(v) = entry.trim().strip_prefix("version=") {
            if let Ok(n) = v.trim().parse::<u8>() {
                return Some(n);
            }
        }
    }
    None
}

/// Maximum `POST /git-upload-pack` body size, both as sent and after decompression.
///
/// Legitimate fetch requests (thousands of `want`/`have` lines) stay well under this; it
/// blocks gzip decompression bombs. Push bodies carry the whole pack, so they are streamed
/// instead and only limited by the repository's `receive.maxInputSize`, like
/// `git http-backend`.
const MAX_FETCH_REQUEST_BODY: usize = 32 * 1024 * 1024;

/// The `Content-Encoding` of a smart-HTTP request body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BodyEncoding {
    Identity,
    Gzip,
}

/// Parse `Content-Encoding`; Git sends either none or `gzip`.
fn body_encoding(headers: &HeaderMap) -> Result<BodyEncoding, Box<Response>> {
    let encoding = headers
        .get(header::CONTENT_ENCODING)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if encoding.eq_ignore_ascii_case("gzip") {
        return Ok(BodyEncoding::Gzip);
    }
    if encoding.is_empty() || encoding.eq_ignore_ascii_case("identity") {
        return Ok(BodyEncoding::Identity);
    }
    Err(Box::new(
        (StatusCode::BAD_REQUEST, "unsupported Content-Encoding").into_response(),
    ))
}

enum GzipDecodeError {
    Invalid,
    TooLarge,
}

/// Read at most `max` bytes from a gzip stream; reject if decompressed size exceeds `max`.
fn read_gzip_with_limit(body: &[u8], max: usize) -> Result<Vec<u8>, GzipDecodeError> {
    use std::io::Read;

    let decoder = flate2::read::GzDecoder::new(body);
    let limit = (max as u64).saturating_add(1);
    let mut limited = decoder.take(limit);
    let mut decoded = Vec::new();
    if limited.read_to_end(&mut decoded).is_err() {
        return Err(GzipDecodeError::Invalid);
    }
    if decoded.len() > max {
        return Err(GzipDecodeError::TooLarge);
    }
    Ok(decoded)
}

fn payload_too_large() -> Box<Response> {
    Box::new((StatusCode::PAYLOAD_TOO_LARGE, "request body too large").into_response())
}

/// Decode a buffered fetch request body (Git may send `Content-Encoding: gzip`).
fn decode_fetch_request_body(headers: &HeaderMap, body: &[u8]) -> Result<Vec<u8>, Box<Response>> {
    match body_encoding(headers)? {
        BodyEncoding::Gzip => {
            read_gzip_with_limit(body, MAX_FETCH_REQUEST_BODY).map_err(|e| match e {
                GzipDecodeError::Invalid => {
                    Box::new((StatusCode::BAD_REQUEST, "invalid gzip request body").into_response())
                }
                GzipDecodeError::TooLarge => payload_too_large(),
            })
        }
        BodyEncoding::Identity if body.len() > MAX_FETCH_REQUEST_BODY => Err(payload_too_large()),
        BodyEncoding::Identity => Ok(body.to_vec()),
    }
}

/// Buffer a fetch request body, refusing anything over [`MAX_FETCH_REQUEST_BODY`].
async fn read_fetch_request_body(body: Body) -> Result<Bytes, Box<Response>> {
    axum::body::to_bytes(body, MAX_FETCH_REQUEST_BODY)
        .await
        .map_err(|e| {
            let inner = e.into_inner();
            let too_large =
                std::iter::successors(Some(&*inner as &(dyn std::error::Error + 'static)), |e| {
                    e.source()
                })
                .any(|e| e.is::<http_body_util::LengthLimitError>());
            if too_large {
                payload_too_large()
            } else {
                Box::new((StatusCode::BAD_REQUEST, "could not read request body").into_response())
            }
        })
}

/// Turn a request body into a blocking reader of its raw bytes, without buffering it.
///
/// Must be called from the async runtime; the returned reader must then be used from a
/// blocking thread (`spawn_blocking`), where each read waits for the next body chunk.
fn streaming_body_reader(body: Body) -> impl Read + Send + 'static {
    use futures_util::TryStreamExt as _;

    let stream = body.into_data_stream().map_err(std::io::Error::other);
    std::io::BufReader::with_capacity(
        STREAM_READ_BUFFER,
        tokio_util::io::SyncIoBridge::new(tokio_util::io::StreamReader::new(stream)),
    )
}

/// Undo the body's `Content-Encoding` on a streaming reader.
///
/// Call this on the blocking thread: the gzip decoder reads its header as soon as it is
/// created, which would block the async runtime.
fn decoded_reader<'a>(raw: impl Read + 'a, encoding: BodyEncoding) -> Box<dyn Read + 'a> {
    match encoding {
        BodyEncoding::Identity => Box::new(raw),
        BodyEncoding::Gzip => Box::new(flate2::read::GzDecoder::new(raw)),
    }
}

/// Read-ahead for streamed request bodies, so pkt-line sized reads do not each wait
/// on the runtime.
const STREAM_READ_BUFFER: usize = 64 * 1024;

fn no_cache_headers(builder: axum::http::response::Builder) -> axum::http::response::Builder {
    builder
        .header(header::EXPIRES, "Fri, 01 Jan 1980 00:00:00 GMT")
        .header(header::PRAGMA, "no-cache")
        .header(
            header::CACHE_CONTROL,
            "no-cache, max-age=0, must-revalidate",
        )
}

// GET /:repo/info/refs?service=git-upload-pack or git-receive-pack
async fn info_refs(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
    Query(query): Query<InfoRefsQuery>,
    protocol_version: Option<u8>,
) -> Response {
    let service = match &query.service {
        Some(s) if s == "git-upload-pack" || s == "git-receive-pack" => s.clone(),
        _ => {
            return (StatusCode::BAD_REQUEST, "unsupported service").into_response();
        }
    };

    let repo_path = match state.repo_path(&repo) {
        Ok(p) => p,
        Err(e) => return *e,
    };

    // Protocol v2 is requested via the `Git-Protocol: version=2` header; absent
    // (or v1), serve the classic v0/v1 advertisement.

    let content_type = format!("application/x-{service}-advertisement");

    let repository_options = state.repository_options.clone();
    let advertisement = match service.as_str() {
        "git-upload-pack" => {
            tokio::task::spawn_blocking(move || {
                grit_protocol::upload_pack::advertise_refs(
                    &repo_path,
                    protocol_version,
                    &repository_options,
                )
            })
            .await
        }
        "git-receive-pack" => {
            tokio::task::spawn_blocking(move || {
                grit_protocol::receive_pack::advertise_refs(&repo_path, &repository_options)
            })
            .await
        }
        _ => unreachable!(),
    };

    let raw_adv = match advertisement {
        Ok(Ok(data)) => data,
        Ok(Err(e)) => {
            eprintln!("info/refs error: {e}");
            return (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response();
        }
        Err(e) => {
            eprintln!("info/refs join error: {e}");
            return (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response();
        }
    };

    // Build the response: pkt-line service header + flush + raw advertisement
    let mut body = Vec::new();
    body.extend_from_slice(&pkt_line(&format!("# service={service}\n")));
    body.extend_from_slice(b"0000"); // flush-pkt
    body.extend_from_slice(&raw_adv);

    state
        .apply_set_cookie(no_cache_headers(axum::http::Response::builder()))
        .header(header::CONTENT_TYPE, content_type)
        .body(axum::body::Body::from(body))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

// POST /:repo/git-upload-pack
async fn upload_pack_rpc(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
    protocol_version: Option<u8>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let repo_path = match state.repo_path(&repo) {
        Ok(p) => p,
        Err(e) => return *e,
    };

    let body = match read_fetch_request_body(body).await {
        Ok(b) => b,
        Err(resp) => return *resp,
    };
    let request = match decode_fetch_request_body(&headers, &body) {
        Ok(b) => b,
        Err(resp) => return *resp,
    };
    let repository_options = state.repository_options.clone();
    match tokio::task::spawn_blocking(move || {
        grit_protocol::upload_pack::stateless_rpc(
            &repo_path,
            &request,
            protocol_version,
            &repository_options,
        )
    })
    .await
    {
        Ok(Ok(data)) => state
            .apply_set_cookie(no_cache_headers(axum::http::Response::builder()))
            .header(header::CONTENT_TYPE, "application/x-git-upload-pack-result")
            .body(axum::body::Body::from(data))
            .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response()),
        Ok(Err(e)) => {
            eprintln!("upload-pack error: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, "upload-pack failed").into_response()
        }
        Err(e) => {
            eprintln!("upload-pack join error: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, "upload-pack failed").into_response()
        }
    }
}

// POST /:repo/git-receive-pack
async fn receive_pack_rpc(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let repo_path = match state.repo_path(&repo) {
        Ok(p) => p,
        Err(e) => return *e,
    };

    let encoding = match body_encoding(&headers) {
        Ok(e) => e,
        Err(resp) => return *resp,
    };
    let raw = streaming_body_reader(body);
    let repository_options = state.repository_options.clone();
    match tokio::task::spawn_blocking(move || {
        grit_protocol::receive_pack::stateless_rpc_from_reader(
            &repo_path,
            &mut decoded_reader(raw, encoding),
            &repository_options,
        )
    })
    .await
    {
        Ok(Ok(data)) => state
            .apply_set_cookie(no_cache_headers(axum::http::Response::builder()))
            .header(
                header::CONTENT_TYPE,
                "application/x-git-receive-pack-result",
            )
            .body(axum::body::Body::from(data))
            .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response()),
        Ok(Err(e)) => {
            eprintln!("receive-pack error: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, "receive-pack failed").into_response()
        }
        Err(e) => {
            eprintln!("receive-pack join error: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, "receive-pack failed").into_response()
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    let root = std::fs::canonicalize(&args.root)?;
    eprintln!("Serving repositories from: {}", root.display());

    let expected_auth = args.require_auth.as_deref().map(expected_basic_auth);
    if expected_auth.is_some() {
        eprintln!("HTTP basic auth required (--require-auth)");
    }

    if args.log_headers.is_some() {
        eprintln!("Logging request headers (--log-headers)");
    }

    let repository_options = grit_protocol::repository_options_from_process();
    let state = Arc::new(AppState {
        root,
        repository_options,
        expected_auth,
        log_headers: args.log_headers,
        set_cookie: args.set_cookie,
    });

    let app = Router::new()
        .route("/{*repo}", get(info_refs_dispatch).post(rpc_dispatch))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(&args.bind).await?;
    eprintln!("Listening on {}", args.bind);
    axum::serve(listener, app).await?;

    Ok(())
}

// Dispatch GET requests — only info/refs is valid
async fn info_refs_dispatch(
    state: State<Arc<AppState>>,
    Path(full_path): Path<String>,
    query: Query<InfoRefsQuery>,
    headers: HeaderMap,
) -> Response {
    state.log_request_headers(&headers);
    if let Some(repo) = full_path.strip_suffix("/info/refs") {
        if repo.is_empty() {
            return (StatusCode::BAD_REQUEST, "missing repository").into_response();
        }
        if let Err(resp) = state.check_auth(&headers) {
            return *resp;
        }
        let protocol_version = protocol_version_from_headers(&headers);
        info_refs(state, Path(repo.to_string()), query, protocol_version).await
    } else {
        (StatusCode::NOT_FOUND, "not found").into_response()
    }
}

// Dispatch POST requests to upload-pack or receive-pack. The body is taken raw
// (not as `Bytes`) so each service decides how to read it: buffered and capped
// for fetch requests, streamed without a cap for pushes.
async fn rpc_dispatch(
    state: State<Arc<AppState>>,
    Path(full_path): Path<String>,
    request: Request,
) -> Response {
    let (parts, body) = request.into_parts();
    let headers = parts.headers;
    state.log_request_headers(&headers);
    if let Err(resp) = state.check_auth(&headers) {
        return *resp;
    }
    if let Some(repo) = full_path.strip_suffix("/git-upload-pack") {
        if repo.is_empty() {
            return (StatusCode::BAD_REQUEST, "missing repository").into_response();
        }
        let protocol_version = protocol_version_from_headers(&headers);
        upload_pack_rpc(
            state,
            Path(repo.to_string()),
            protocol_version,
            headers,
            body,
        )
        .await
    } else if let Some(repo) = full_path.strip_suffix("/git-receive-pack") {
        if repo.is_empty() {
            return (StatusCode::BAD_REQUEST, "missing repository").into_response();
        }
        receive_pack_rpc(state, Path(repo.to_string()), headers, body).await
    } else {
        (StatusCode::NOT_FOUND, "not found").into_response()
    }
}

#[cfg(test)]
mod body_tests {
    use super::*;
    use axum::http::HeaderValue;
    use flate2::write::GzEncoder;
    use flate2::Compression;
    use std::io::Write;

    fn gzip_bytes(plain: &[u8]) -> Vec<u8> {
        let mut enc = GzEncoder::new(Vec::new(), Compression::fast());
        assert!(enc.write_all(plain).is_ok(), "gzip compress setup");
        match enc.finish() {
            Ok(v) => v,
            Err(e) => panic!("gzip finish failed: {e}"),
        }
    }

    fn gzip_content_encoding_header() -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(header::CONTENT_ENCODING, HeaderValue::from_static("gzip"));
        headers
    }

    #[test]
    fn gzip_smart_http_post_body_is_decoded() {
        let plain = b"0012command=fetch\n";
        let gzip = gzip_bytes(plain);
        let headers = gzip_content_encoding_header();
        let out = match decode_fetch_request_body(&headers, &gzip) {
            Ok(v) => v,
            Err(_) => panic!("decode gzip body"),
        };
        assert_eq!(out, plain);
    }

    #[test]
    fn gzip_decoded_body_over_limit_returns_payload_too_large() {
        let plain = vec![0u8; MAX_FETCH_REQUEST_BODY + 1];
        let gzip = gzip_bytes(&plain);
        let headers = gzip_content_encoding_header();
        match decode_fetch_request_body(&headers, &gzip) {
            Err(resp) => assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE),
            Ok(_) => panic!("oversized gzip must fail"),
        }
    }

    #[test]
    fn identity_body_over_limit_returns_payload_too_large() {
        let body = vec![0u8; MAX_FETCH_REQUEST_BODY + 1];
        let headers = HeaderMap::new();
        match decode_fetch_request_body(&headers, &body) {
            Err(resp) => assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE),
            Ok(_) => panic!("oversized body must fail"),
        }
    }

    #[test]
    fn unknown_content_encoding_is_rejected() {
        let mut headers = HeaderMap::new();
        headers.insert(header::CONTENT_ENCODING, HeaderValue::from_static("br"));
        match body_encoding(&headers) {
            Err(resp) => assert_eq!(resp.status(), StatusCode::BAD_REQUEST),
            Ok(e) => panic!("br must be rejected, got {e:?}"),
        }
    }

    #[tokio::test]
    async fn fetch_request_body_over_limit_returns_payload_too_large() {
        let body = Body::from(vec![0u8; MAX_FETCH_REQUEST_BODY + 1]);
        match read_fetch_request_body(body).await {
            Err(resp) => assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE),
            Ok(_) => panic!("oversized fetch request must fail"),
        }
    }

    /// A body of many chunks, larger than the fetch cap, as hyper delivers a push.
    fn chunked_body(chunks: Vec<Vec<u8>>) -> Body {
        let stream = futures_util::stream::iter(
            chunks
                .into_iter()
                .map(|c| Ok::<_, std::io::Error>(Bytes::from(c))),
        );
        Body::from_stream(stream)
    }

    async fn read_streamed(body: Body, encoding: BodyEncoding) -> Vec<u8> {
        let raw = streaming_body_reader(body);
        let read = tokio::task::spawn_blocking(move || {
            let mut out = Vec::new();
            decoded_reader(raw, encoding)
                .read_to_end(&mut out)
                .map(|_| out)
        });
        match read.await {
            Ok(Ok(out)) => out,
            Ok(Err(e)) => panic!("read streamed body: {e}"),
            Err(e) => panic!("join: {e}"),
        }
    }

    #[tokio::test]
    async fn streamed_identity_body_is_not_capped() {
        let chunk: Vec<u8> = (0..=255u8).cycle().take(1024 * 1024).collect();
        let chunks = vec![chunk.clone(); MAX_FETCH_REQUEST_BODY / chunk.len() + 2];
        let expected = chunks.concat();
        let out = read_streamed(chunked_body(chunks), BodyEncoding::Identity).await;
        assert_eq!(out.len(), expected.len());
        assert!(out == expected, "streamed bytes must match the body");
    }

    #[tokio::test]
    async fn streamed_gzip_body_is_decoded() {
        let plain: Vec<u8> = (0..3_000_000u32).map(|i| (i % 251) as u8).collect();
        let gzip = gzip_bytes(&plain);
        let chunks = gzip.chunks(4096).map(<[u8]>::to_vec).collect();
        let out = read_streamed(chunked_body(chunks), BodyEncoding::Gzip).await;
        assert!(
            out == plain,
            "gzip push body must decode to the original bytes"
        );
    }
}
