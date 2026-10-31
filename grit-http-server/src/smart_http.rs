//! Smart HTTP endpoints for a single repository.
//!
//! Smart HTTP needs four endpoints per repository:
//!
//! - `GET  <repo>/info/refs?service=git-upload-pack`
//! - `GET  <repo>/info/refs?service=git-receive-pack`
//! - `POST <repo>/git-upload-pack`
//! - `POST <repo>/git-receive-pack`
//!
//! [`info_refs`] answers the first two and [`rpc`] the last two. Neither does
//! routing or authentication: callers resolve the repository path and decide
//! whether the client may use the [`Service`] first.
//!
//! Fetch responses stream: upload-pack writes into a bounded channel the
//! response body reads from, so a clone of any size takes about
//! [`STREAM_CHUNK`] × [`STREAM_CHANNEL_DEPTH`] of buffer on the server, and a
//! client that reads slowly slows the pack down instead of piling it up.

use std::io::Read;
use std::path::PathBuf;

use axum::{
    body::{Body, Bytes},
    http::{header, response::Builder, HeaderMap, Response, StatusCode},
    response::IntoResponse,
};
use grit_protocol::RepositoryOptions;

/// The two smart HTTP services.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Service {
    /// Fetch and clone (`git-upload-pack`).
    UploadPack,
    /// Push (`git-receive-pack`).
    ReceivePack,
}

impl Service {
    /// Parse the `service=` query value of an `info/refs` request.
    ///
    /// Returns `None` for anything other than `git-upload-pack` or
    /// `git-receive-pack` (including the dumb-HTTP request with no service).
    #[must_use]
    pub fn from_query(service: &str) -> Option<Self> {
        match service {
            "git-upload-pack" => Some(Self::UploadPack),
            "git-receive-pack" => Some(Self::ReceivePack),
            _ => None,
        }
    }

    /// The wire name: `git-upload-pack` or `git-receive-pack`.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::UploadPack => "git-upload-pack",
            Self::ReceivePack => "git-receive-pack",
        }
    }

    /// Split a request path ending in `/git-upload-pack` or `/git-receive-pack`
    /// into the repository part and the service.
    #[must_use]
    pub fn strip_rpc_suffix(path: &str) -> Option<(&str, Self)> {
        if let Some(repo) = path.strip_suffix("/git-upload-pack") {
            return Some((repo, Self::UploadPack));
        }
        path.strip_suffix("/git-receive-pack")
            .map(|repo| (repo, Self::ReceivePack))
    }
}

/// Maximum decoded size of a `POST <repo>/git-upload-pack` body.
///
/// A clone of a repository with millions of refs sends one `want` line per ref; those
/// requests are streamed and counted as they are decoded. Push bodies carry the whole pack,
/// so they are streamed separately and only limited by the repository's
/// `receive.maxInputSize`, like `git http-backend`.
pub const MAX_FETCH_REQUEST_BODY: usize = 1024 * 1024 * 1024;

/// Parse the requested protocol version from the `Git-Protocol` request header.
///
/// Git sends `Git-Protocol: version=2` to request protocol v2 over smart HTTP;
/// the header carries colon-separated key=value entries (e.g.
/// `version=2:agent=git/2`). Returns `None` (the classic v0/v1 advertisement
/// and stateless RPC) when the header is absent or names no version.
#[must_use]
pub fn protocol_version_from_headers(headers: &HeaderMap) -> Option<u8> {
    let raw = headers.get("Git-Protocol")?.to_str().ok()?;
    raw.split([':', ' '])
        .filter_map(|entry| entry.trim().strip_prefix("version="))
        .find_map(|v| v.trim().parse::<u8>().ok())
}

/// Answer `GET <repo>/info/refs?service=…` for the repository at `repo_path`.
///
/// The body is the pkt-line `# service=…` header, a flush packet, then the
/// service's advertisement (the protocol v2 capability list when
/// `protocol_version` is `Some(2)` for upload-pack).
///
/// # Parameters
///
/// - `repo_path`: an already-resolved repository directory.
/// - `service`: which advertisement to produce.
/// - `protocol_version`: from [`protocol_version_from_headers`].
/// - `options`: environment used to load global and system config.
/// - `extensions`: a `bundle-uri` bundle list or `pushBundle` endpoint to
///   advertise.
///
/// Failures become a `500` response rather than an error.
pub async fn info_refs(
    repo_path: PathBuf,
    service: Service,
    protocol_version: Option<u8>,
    options: RepositoryOptions,
    extensions: grit_protocol::Extensions,
) -> Response<Body> {
    let advertisement = tokio::task::spawn_blocking(move || match service {
        Service::UploadPack => grit_protocol::upload_pack::advertise_refs_with(
            &repo_path,
            protocol_version,
            &options,
            &extensions,
        ),
        Service::ReceivePack => {
            grit_protocol::receive_pack::advertise_refs_with(&repo_path, &options, &extensions)
        }
    })
    .await;
    let advertisement = match advertisement {
        Ok(Ok(data)) => data,
        Ok(Err(e)) => return internal_error(&format!("info/refs failed: {e}")),
        Err(e) => return internal_error(&format!("info/refs failed: {e}")),
    };

    let mut body = pkt_line(&format!("# service={}\n", service.name()));
    body.extend_from_slice(b"0000");
    body.extend_from_slice(&advertisement);
    no_cache_headers(Response::builder())
        .header(
            header::CONTENT_TYPE,
            format!("application/x-{}-advertisement", service.name()),
        )
        .body(Body::from(body))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

/// Answer `POST <repo>/git-upload-pack` or `POST <repo>/git-receive-pack`.
///
/// Undoes `Content-Encoding: gzip` on the request body, then runs one stateless
/// exchange of `service` against the repository at `repo_path`. Fetch request
/// bodies are streamed and capped at [`MAX_FETCH_REQUEST_BODY`] decoded bytes;
/// push bodies carry the whole pack and are streamed to receive-pack without
/// that cap.
///
/// # Parameters
///
/// - `repo_path`: an already-resolved repository directory.
/// - `service`: which program answers the request.
/// - `protocol_version`: from [`protocol_version_from_headers`]; push always
///   uses protocol v0.
/// - `headers`, `body`: the HTTP request.
/// - `options`: environment used to load global and system config.
/// - `extensions`: the bundle list `command=bundle-uri` answers with.
///
/// Bad bodies become `400`/`413` and failures `500` responses.
pub async fn rpc(
    repo_path: PathBuf,
    service: Service,
    protocol_version: Option<u8>,
    headers: &HeaderMap,
    body: Body,
    options: RepositoryOptions,
    extensions: grit_protocol::Extensions,
) -> Response<Body> {
    rpc_holding(
        repo_path,
        service,
        protocol_version,
        headers,
        body,
        options,
        extensions,
        (),
    )
    .await
}

/// [`rpc`], keeping `hold` alive until the response body has been sent in
/// full (or the client went away): a fetch response streams after this
/// returns, so whatever keeps the repository readable (a lock or lease) must
/// live as long as the body.
#[allow(clippy::too_many_arguments)]
pub async fn rpc_holding<H: Send + 'static>(
    repo_path: PathBuf,
    service: Service,
    protocol_version: Option<u8>,
    headers: &HeaderMap,
    body: Body,
    options: RepositoryOptions,
    extensions: grit_protocol::Extensions,
    hold: H,
) -> Response<Body> {
    let encoding = match body_encoding(headers) {
        Ok(e) => e,
        Err(resp) => return *resp,
    };
    let data = match service {
        Service::UploadPack => {
            let raw = streaming_body_reader(body);
            return stream_upload_pack(
                repo_path,
                raw,
                encoding,
                protocol_version,
                options,
                extensions,
                hold,
            )
            .await;
        }
        Service::ReceivePack => {
            let raw = streaming_body_reader(body);
            let result = tokio::task::spawn_blocking(move || {
                grit_protocol::receive_pack::stateless_rpc_from_reader(
                    &repo_path,
                    &mut decoded_reader(raw, encoding),
                    &options,
                )
            })
            .await;
            match result {
                Ok(Ok(data)) => data,
                Ok(Err(e)) => return receive_pack_error_response(&e),
                Err(e) => return internal_error(&format!("{} failed: {e}", service.name())),
            }
        }
    };
    no_cache_headers(Response::builder())
        .header(
            header::CONTENT_TYPE,
            format!("application/x-{}-result", service.name()),
        )
        .body(Body::from(data))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

/// Answer `GET` for a file that clients download with resumable `Range`
/// requests, such as a clone bundle.
///
/// Sends `ETag` and `Accept-Ranges: bytes`. A `Range: bytes=<start>-[<end>]`
/// header gets `206` with that part (`416` when it starts past the end), unless
/// `If-Range` names another version, which gets the whole file.
///
/// # Parameters
///
/// - `path`: the file.
/// - `etag`: a strong validator for this version of the file.
/// - `content_type`: the `Content-Type` to send.
/// - `headers`: the request headers.
pub async fn serve_file(
    path: PathBuf,
    etag: &str,
    content_type: &str,
    headers: &HeaderMap,
) -> Response<Body> {
    let mut file = match tokio::fs::File::open(&path).await {
        Ok(f) => f,
        Err(_) => return (StatusCode::NOT_FOUND, "not found\n").into_response(),
    };
    let size = match file.metadata().await {
        Ok(m) => m.len(),
        Err(e) => return internal_error(&e.to_string()),
    };
    let if_range_matches = headers
        .get(header::IF_RANGE)
        .and_then(|v| v.to_str().ok())
        .is_none_or(|v| v == etag);
    let range = headers
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .filter(|_| if_range_matches)
        .and_then(parse_byte_range);
    let builder = Response::builder()
        .header(header::CONTENT_TYPE, content_type)
        .header(header::ETAG, etag)
        .header(header::ACCEPT_RANGES, "bytes")
        .header(
            header::CACHE_CONTROL,
            "private, max-age=31536000, immutable",
        );
    let (builder, start, len) = match range {
        None => (builder.status(StatusCode::OK), 0, size),
        Some((start, _)) if start >= size => {
            return Response::builder()
                .status(StatusCode::RANGE_NOT_SATISFIABLE)
                .header(header::CONTENT_RANGE, format!("bytes */{size}"))
                .body(Body::empty())
                .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response());
        }
        Some((start, end)) => {
            let end = end.unwrap_or(size - 1).min(size - 1);
            if end < start {
                return (StatusCode::RANGE_NOT_SATISFIABLE, "bad range\n").into_response();
            }
            (
                builder
                    .status(StatusCode::PARTIAL_CONTENT)
                    .header(header::CONTENT_RANGE, format!("bytes {start}-{end}/{size}")),
                start,
                end - start + 1,
            )
        }
    };
    use tokio::io::{AsyncReadExt as _, AsyncSeekExt as _};
    if let Err(e) = file.seek(std::io::SeekFrom::Start(start)).await {
        return internal_error(&e.to_string());
    }
    let stream = tokio_util::io::ReaderStream::new(file.take(len));
    builder
        .header(header::CONTENT_LENGTH, len.to_string())
        .body(Body::from_stream(stream))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

/// Parse a single `bytes=<start>-[<end>]` range. Suffix ranges (`bytes=-<n>`)
/// and multiple ranges are not supported and are answered with the whole file.
fn parse_byte_range(value: &str) -> Option<(u64, Option<u64>)> {
    let spec = value.trim().strip_prefix("bytes=")?;
    if spec.contains(',') {
        return None;
    }
    let (start, end) = spec.split_once('-')?;
    let start = start.trim().parse().ok()?;
    let end = match end.trim() {
        "" => None,
        e => Some(e.parse().ok()?),
    };
    Some((start, end))
}

fn internal_error(message: &str) -> Response<Body> {
    (StatusCode::INTERNAL_SERVER_ERROR, message.to_owned()).into_response()
}

fn pkt_line(data: &str) -> Vec<u8> {
    let len = data.len() + 4;
    format!("{len:04x}{data}").into_bytes()
}

fn no_cache_headers(builder: Builder) -> Builder {
    builder
        .header(header::EXPIRES, "Fri, 01 Jan 1980 00:00:00 GMT")
        .header(header::PRAGMA, "no-cache")
        .header(
            header::CACHE_CONTROL,
            "no-cache, max-age=0, must-revalidate",
        )
}

/// The `Content-Encoding` of a smart-HTTP request body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BodyEncoding {
    Identity,
    Gzip,
}

/// Parse `Content-Encoding`; Git sends either none or `gzip`.
fn body_encoding(headers: &HeaderMap) -> Result<BodyEncoding, Box<Response<Body>>> {
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

fn payload_too_large() -> Response<Body> {
    (StatusCode::PAYLOAD_TOO_LARGE, "request body too large").into_response()
}

/// A fetch request body expanded past [`MAX_FETCH_REQUEST_BODY`].
#[derive(Debug)]
struct FetchBodyTooLarge;

impl std::fmt::Display for FetchBodyTooLarge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "fetch request body exceeds the decoded size limit")
    }
}

impl std::error::Error for FetchBodyTooLarge {}

/// Counts decoded bytes and fails once the limit is exceeded.
struct DecodedByteLimit<R> {
    inner: R,
    decoded: usize,
    max: usize,
}

impl<R: Read> Read for DecodedByteLimit<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.decoded >= self.max {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                FetchBodyTooLarge,
            ));
        }
        let n = self.inner.read(buf)?;
        self.decoded = self.decoded.saturating_add(n);
        if self.decoded > self.max {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                FetchBodyTooLarge,
            ));
        }
        Ok(n)
    }
}

/// Gzip decoder for fetch bodies, limited by decoded size only.
struct FetchGzip<R: Read> {
    decoder: flate2::read::GzDecoder<R>,
    decoded: usize,
    max: usize,
}

impl<R: Read> Read for FetchGzip<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.decoded >= self.max {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                FetchBodyTooLarge,
            ));
        }
        let n = self.decoder.read(buf)?;
        self.decoded = self.decoded.saturating_add(n);
        if self.decoded > self.max {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                FetchBodyTooLarge,
            ));
        }
        Ok(n)
    }
}

/// Apply fetch-body size limits while undoing `Content-Encoding`.
///
/// Call on the blocking thread: the gzip decoder reads its header as soon as it is created.
fn decoded_fetch_reader(raw: impl Read + Send + 'static, encoding: BodyEncoding) -> Box<dyn Read> {
    let max = MAX_FETCH_REQUEST_BODY;
    match encoding {
        BodyEncoding::Identity => Box::new(DecodedByteLimit {
            inner: raw,
            decoded: 0,
            max,
        }),
        BodyEncoding::Gzip => Box::new(FetchGzip {
            decoder: flate2::read::GzDecoder::new(raw),
            decoded: 0,
            max,
        }),
    }
}

/// Read-ahead for streamed request bodies, so pkt-line sized reads do not each wait
/// on the runtime.
const STREAM_READ_BUFFER: usize = 64 * 1024;

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
        BodyEncoding::Gzip => Box::new(ExpansionLimitedGzip::new(raw)),
    }
}

/// Decoded bytes a streamed gzip push body may always expand to before the ratio check.
const GZIP_STREAM_FREE_EXPANSION: u64 = 32 * 1024 * 1024;

/// Past [`GZIP_STREAM_FREE_EXPANSION`], how many decoded bytes a streamed gzip body may
/// produce per compressed byte received.
///
/// A push carries a pack whose objects are already deflated, so gzip barely shrinks it;
/// even a pack written without compression rarely gzips better than 10:1. Deflate can
/// reach about 1000:1, so without this bound a small request could grow without limit.
const GZIP_STREAM_MAX_RATIO: u64 = 32;

/// A streamed gzip body expanded beyond [`GZIP_STREAM_MAX_RATIO`].
#[derive(Debug)]
struct GzipExpansionExceeded;

impl std::fmt::Display for GzipExpansionExceeded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "gzip request body expands more than {GZIP_STREAM_MAX_RATIO}:1"
        )
    }
}

impl std::error::Error for GzipExpansionExceeded {}

/// Counts the bytes read through it.
struct CountingReader<R> {
    inner: R,
    count: u64,
}

impl<R: Read> Read for CountingReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.count = self.count.saturating_add(n as u64);
        Ok(n)
    }
}

/// Gzip decoder for streamed bodies that fails with [`GzipExpansionExceeded`] once the
/// decoded output outgrows the compressed input by more than the allowed ratio.
struct ExpansionLimitedGzip<R: Read> {
    decoder: flate2::read::GzDecoder<CountingReader<R>>,
    decoded: u64,
}

impl<R: Read> ExpansionLimitedGzip<R> {
    fn new(inner: R) -> Self {
        Self {
            decoder: flate2::read::GzDecoder::new(CountingReader { inner, count: 0 }),
            decoded: 0,
        }
    }
}

impl<R: Read> Read for ExpansionLimitedGzip<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.decoder.read(buf)?;
        self.decoded = self.decoded.saturating_add(n as u64);
        let compressed = self.decoder.get_ref().count;
        let allowed = compressed
            .saturating_mul(GZIP_STREAM_MAX_RATIO)
            .max(GZIP_STREAM_FREE_EXPANSION);
        if self.decoded > allowed {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                GzipExpansionExceeded,
            ));
        }
        Ok(n)
    }
}

/// The HTTP response for a receive-pack exchange that failed.
///
/// A request stream that cannot be read (malformed or over-expanding gzip, a client that
/// went away) is the client's fault; anything else is a server error.
/// The HTTP response for an upload-pack exchange that failed before any output.
fn upload_pack_error_response(err: &grit_protocol::Error) -> Response<Body> {
    let Some(io) = err.request_stream_error() else {
        return internal_error(&format!("git-upload-pack failed: {err}"));
    };
    if io
        .get_ref()
        .is_some_and(|inner| inner.is::<FetchBodyTooLarge>())
    {
        return payload_too_large();
    }
    (StatusCode::BAD_REQUEST, "could not read request body").into_response()
}

fn receive_pack_error_response(err: &grit_protocol::Error) -> Response<Body> {
    let Some(io) = err.request_stream_error() else {
        return internal_error(&format!("git-receive-pack failed: {err}"));
    };
    if io
        .get_ref()
        .is_some_and(|inner| inner.is::<GzipExpansionExceeded>())
    {
        return payload_too_large();
    }
    (StatusCode::BAD_REQUEST, "could not read request body").into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;
    use flate2::write::GzEncoder;
    use flate2::Compression;
    use std::io::Write;

    /// Decode a fetch body the way [`rpc`] does for upload-pack requests.
    fn decode_fetch(headers: &HeaderMap, body: &[u8]) -> std::io::Result<Vec<u8>> {
        let encoding = body_encoding(headers).map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "bad encoding header")
        })?;
        let mut out = Vec::new();
        decoded_fetch_reader(std::io::Cursor::new(body.to_vec()), encoding)
            .read_to_end(&mut out)?;
        Ok(out)
    }

    fn want_pkt_line(oid: &str) -> String {
        let line = format!("want {oid}\n");
        format!("{:04x}{line}", line.len() + 4)
    }

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
        let out = decode_fetch(&headers, &gzip).expect("decode gzip body");
        assert_eq!(out, plain);
    }

    #[test]
    fn gzip_decoded_body_over_limit_returns_fetch_body_too_large() {
        let plain = vec![0u8; MAX_FETCH_REQUEST_BODY + 1];
        let gzip = gzip_bytes(&plain);
        let headers = gzip_content_encoding_header();
        let err = decode_fetch(&headers, &gzip).expect_err("oversized gzip must fail");
        assert!(
            err.get_ref()
                .is_some_and(|inner| inner.is::<FetchBodyTooLarge>()),
            "expected FetchBodyTooLarge, got {err}"
        );
    }

    #[test]
    fn identity_body_over_limit_returns_fetch_body_too_large() {
        let body = vec![0u8; MAX_FETCH_REQUEST_BODY + 1];
        let headers = HeaderMap::new();
        let err = decode_fetch(&headers, &body).expect_err("oversized body must fail");
        assert!(
            err.get_ref()
                .is_some_and(|inner| inner.is::<FetchBodyTooLarge>()),
            "expected FetchBodyTooLarge, got {err}"
        );
    }

    #[test]
    fn large_want_body_under_limit_decodes() {
        let oid = "a".repeat(40);
        let pkt = want_pkt_line(&oid);
        let mut body = String::new();
        while body.len() < 40 * 1024 * 1024 {
            body.push_str(&pkt);
        }
        body.push_str("0000");
        let headers = HeaderMap::new();
        let out = decode_fetch(&headers, body.as_bytes()).expect("large want body");
        assert_eq!(out.len(), body.len());
    }

    #[test]
    fn gzip_repetitive_want_lines_over_32mib_decode_under_fetch_cap() {
        let oid = "b".repeat(40);
        let pkt = want_pkt_line(&oid);
        let mut plain = String::new();
        while plain.len() < 35 * 1024 * 1024 {
            plain.push_str(&pkt);
        }
        plain.push_str("0000");
        let gzip = gzip_bytes(plain.as_bytes());
        let headers = gzip_content_encoding_header();
        let out = decode_fetch(&headers, &gzip).expect("gzip want flood");
        assert!(out.len() > 32 * 1024 * 1024);
        assert!(out.len() < MAX_FETCH_REQUEST_BODY);
    }

    #[test]
    fn byte_ranges_parse() {
        assert_eq!(parse_byte_range("bytes=10-"), Some((10, None)));
        assert_eq!(parse_byte_range("bytes=10-20"), Some((10, Some(20))));
        assert_eq!(parse_byte_range("bytes=-20"), None);
        assert_eq!(parse_byte_range("bytes=1-2,4-5"), None);
        assert_eq!(parse_byte_range("items=1-"), None);
    }

    #[test]
    fn protocol_version_header_is_parsed() {
        let mut headers = HeaderMap::new();
        assert_eq!(protocol_version_from_headers(&headers), None);
        headers.insert(
            "Git-Protocol",
            HeaderValue::from_static("agent=git/2:version=2"),
        );
        assert_eq!(protocol_version_from_headers(&headers), Some(2));
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
    async fn streamed_fetch_body_over_limit_fails() {
        let chunk: Vec<u8> = (0..=255u8).cycle().take(1024 * 1024).collect();
        let chunks = vec![chunk; MAX_FETCH_REQUEST_BODY / (1024 * 1024) + 2];
        let read = tokio::task::spawn_blocking(move || {
            let raw = streaming_body_reader(chunked_body(chunks));
            let mut out = Vec::new();
            decoded_fetch_reader(raw, BodyEncoding::Identity).read_to_end(&mut out)
        });
        let err = match read.await {
            Ok(Err(e)) => e,
            Ok(Ok(_)) => panic!("oversized fetch request must fail"),
            Err(e) => panic!("join: {e}"),
        };
        assert!(
            err.get_ref()
                .is_some_and(|inner| inner.is::<FetchBodyTooLarge>()),
            "expected FetchBodyTooLarge, got {err}"
        );
    }

    /// A body of many chunks, larger than the push gzip free allowance, as hyper delivers a push.
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
        let chunks = vec![chunk.clone(); (GZIP_STREAM_FREE_EXPANSION as usize) / chunk.len() + 2];
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

    #[test]
    fn gzip_stream_expanding_past_ratio_fails_with_typed_error() {
        let zeros = vec![0u8; 1024 * 1024];
        let mut enc = GzEncoder::new(Vec::new(), Compression::default());
        for _ in 0..64 {
            assert!(enc.write_all(&zeros).is_ok(), "gzip compress setup");
        }
        let bomb = match enc.finish() {
            Ok(v) => v,
            Err(e) => panic!("gzip finish failed: {e}"),
        };

        let mut reader = decoded_reader(&bomb[..], BodyEncoding::Gzip);
        let mut buf = vec![0u8; 64 * 1024];
        let mut decoded = 0u64;
        let err = loop {
            match reader.read(&mut buf) {
                Ok(0) => panic!("bomb decoded completely ({decoded} bytes)"),
                Ok(n) => decoded += n as u64,
                Err(e) => break e,
            }
        };
        assert!(
            err.get_ref()
                .is_some_and(|inner| inner.is::<GzipExpansionExceeded>()),
            "expected GzipExpansionExceeded, got {err}"
        );
        assert!(
            decoded <= GZIP_STREAM_FREE_EXPANSION,
            "decoding must stop at the free allowance, got {decoded} bytes"
        );
    }

    #[test]
    fn gzip_stream_with_low_ratio_is_not_capped() {
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let plain: Vec<u8> = (0..(GZIP_STREAM_FREE_EXPANSION as usize + 8 * 1024 * 1024) / 8)
            .flat_map(|_| {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                seed.to_le_bytes()
            })
            .collect();
        let gzip = gzip_bytes(&plain);
        let mut out = Vec::new();
        if let Err(e) = decoded_reader(&gzip[..], BodyEncoding::Gzip).read_to_end(&mut out) {
            panic!("incompressible gzip stream must decode: {e}");
        }
        assert!(out == plain, "decoded bytes must match");
    }

    #[test]
    fn services_round_trip_their_names() {
        for service in [Service::UploadPack, Service::ReceivePack] {
            assert_eq!(Service::from_query(service.name()), Some(service));
        }
        assert_eq!(Service::from_query("git-archive"), None);
        assert_eq!(
            Service::strip_rpc_suffix("a/b.git/git-receive-pack"),
            Some(("a/b.git", Service::ReceivePack))
        );
        assert_eq!(Service::strip_rpc_suffix("a/info/refs"), None);
    }
}

/// Bytes upload-pack output is gathered into before it is handed to the body.
pub const STREAM_CHUNK: usize = 64 * 1024;

/// Chunks that may wait between upload-pack and a slow client.
pub const STREAM_CHANNEL_DEPTH: usize = 8;

type Chunk = std::io::Result<Bytes>;

/// A `Write` for a blocking thread that hands what is written to an async
/// response body in [`STREAM_CHUNK`]s, waiting while the body is
/// [`STREAM_CHANNEL_DEPTH`] chunks behind. Once the body is gone (the client
/// disconnected), writes fail with `BrokenPipe`, which stops upload-pack.
struct ChannelWriter {
    pending: Vec<u8>,
    tx: tokio::sync::mpsc::Sender<Chunk>,
    sent: bool,
}

impl ChannelWriter {
    fn new(tx: tokio::sync::mpsc::Sender<Chunk>) -> Self {
        Self {
            pending: Vec::with_capacity(STREAM_CHUNK),
            tx,
            sent: false,
        }
    }

    fn send_pending(&mut self) -> std::io::Result<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        let chunk = Bytes::from(std::mem::replace(
            &mut self.pending,
            Vec::with_capacity(STREAM_CHUNK),
        ));
        self.tx
            .blocking_send(Ok(chunk))
            .map_err(|_| std::io::Error::from(std::io::ErrorKind::BrokenPipe))?;
        self.sent = true;
        Ok(())
    }
}

impl std::io::Write for ChannelWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let room = STREAM_CHUNK.saturating_sub(self.pending.len()).max(1);
        let n = buf.len().min(room);
        self.pending.extend_from_slice(&buf[..n]);
        if self.pending.len() >= STREAM_CHUNK {
            self.send_pending()?;
        }
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.send_pending()
    }
}

/// Run upload-pack on a blocking thread and stream its output as the
/// response body.
///
/// The status is decided by the first chunk: a failure before any output is
/// a `500` as before; once output has started the response is a `200`, and a
/// later failure aborts the body (the client sees a truncated transfer and
/// fails) instead of ending it as if the pack were complete.
async fn stream_upload_pack<H: Send + 'static>(
    repo_path: PathBuf,
    raw: impl Read + Send + 'static,
    encoding: BodyEncoding,
    protocol_version: Option<u8>,
    options: RepositoryOptions,
    extensions: grit_protocol::Extensions,
    hold: H,
) -> Response<Body> {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Chunk>(STREAM_CHANNEL_DEPTH);
    let task = tokio::task::spawn_blocking(move || {
        let mut request = decoded_fetch_reader(raw, encoding);
        let mut out = ChannelWriter::new(tx);
        let result = grit_protocol::upload_pack::stateless_rpc_to_reader(
            &repo_path,
            &mut request,
            protocol_version,
            &options,
            &extensions,
            &mut out,
        );
        match result {
            Ok(()) => out.send_pending().map_err(grit_protocol::Error::Io),
            Err(e) => {
                if out.sent {
                    // Part of the response is out: fail the body, not the status.
                    let _ = out.tx.blocking_send(Err(std::io::Error::other(format!(
                        "upload-pack failed: {e}"
                    ))));
                }
                Err(e)
            }
        }
    });
    let first = match rx.recv().await {
        Some(Ok(first)) => first,
        Some(Err(e)) => return internal_error(&format!("upload-pack failed: {e}")),
        // Nothing was sent: the task is done, one way or the other.
        None => match task.await {
            Ok(Ok(())) => Bytes::new(),
            Ok(Err(e)) => return upload_pack_error_response(&e),
            Err(e) => return internal_error(&format!("upload-pack failed: {e}")),
        },
    };
    let rest = futures_util::stream::unfold((rx, hold), |(mut rx, hold)| async move {
        rx.recv().await.map(|chunk| (chunk, (rx, hold)))
    });
    let body = futures_util::StreamExt::chain(
        futures_util::stream::once(std::future::ready(Ok::<Bytes, std::io::Error>(first))),
        rest,
    );
    no_cache_headers(Response::builder())
        .header(header::CONTENT_TYPE, "application/x-git-upload-pack-result")
        .body(Body::from_stream(body))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}
