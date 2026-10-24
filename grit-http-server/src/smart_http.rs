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

/// Maximum `POST <repo>/git-upload-pack` body size, both as sent and after decompression.
///
/// Legitimate fetch requests (thousands of `want`/`have` lines) stay well under this; it
/// blocks gzip decompression bombs. Push bodies carry the whole pack, so they are streamed
/// instead and only limited by the repository's `receive.maxInputSize`, like
/// `git http-backend`.
pub const MAX_FETCH_REQUEST_BODY: usize = 32 * 1024 * 1024;

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
///
/// Failures become a `500` response rather than an error.
pub async fn info_refs(
    repo_path: PathBuf,
    service: Service,
    protocol_version: Option<u8>,
    options: RepositoryOptions,
) -> Response<Body> {
    let advertisement = tokio::task::spawn_blocking(move || match service {
        Service::UploadPack => {
            grit_protocol::upload_pack::advertise_refs(&repo_path, protocol_version, &options)
        }
        Service::ReceivePack => grit_protocol::receive_pack::advertise_refs(&repo_path, &options),
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
/// exchange of `service` against the repository at `repo_path`. Fetch requests
/// are buffered and capped at [`MAX_FETCH_REQUEST_BODY`]; push bodies carry the
/// whole pack, so they are streamed to receive-pack without a cap.
///
/// # Parameters
///
/// - `repo_path`: an already-resolved repository directory.
/// - `service`: which program answers the request.
/// - `protocol_version`: from [`protocol_version_from_headers`]; push always
///   uses protocol v0.
/// - `headers`, `body`: the HTTP request.
/// - `options`: environment used to load global and system config.
///
/// Bad bodies become `400`/`413` and failures `500` responses.
pub async fn rpc(
    repo_path: PathBuf,
    service: Service,
    protocol_version: Option<u8>,
    headers: &HeaderMap,
    body: Body,
    options: RepositoryOptions,
) -> Response<Body> {
    let encoding = match body_encoding(headers) {
        Ok(e) => e,
        Err(resp) => return *resp,
    };
    let data = match service {
        Service::UploadPack => {
            let body = match read_fetch_request_body(body).await {
                Ok(b) => b,
                Err(resp) => return *resp,
            };
            let request = match decode_fetch_request_body(encoding, &body) {
                Ok(b) => b,
                Err(resp) => return *resp,
            };
            let result = tokio::task::spawn_blocking(move || {
                grit_protocol::upload_pack::stateless_rpc(
                    &repo_path,
                    &request,
                    protocol_version,
                    &options,
                )
            })
            .await;
            match result {
                Ok(Ok(data)) => data,
                Ok(Err(e)) => return internal_error(&format!("{} failed: {e}", service.name())),
                Err(e) => return internal_error(&format!("{} failed: {e}", service.name())),
            }
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

enum GzipDecodeError {
    Invalid,
    TooLarge,
}

/// Read at most `max` bytes from a gzip stream; reject if decompressed size exceeds `max`.
fn read_gzip_with_limit(body: &[u8], max: usize) -> Result<Vec<u8>, GzipDecodeError> {
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

fn payload_too_large() -> Response<Body> {
    (StatusCode::PAYLOAD_TOO_LARGE, "request body too large").into_response()
}

/// Decode a buffered fetch request body.
fn decode_fetch_request_body(
    encoding: BodyEncoding,
    body: &[u8],
) -> Result<Vec<u8>, Box<Response<Body>>> {
    match encoding {
        BodyEncoding::Gzip => read_gzip_with_limit(body, MAX_FETCH_REQUEST_BODY).map_err(|e| {
            Box::new(match e {
                GzipDecodeError::Invalid => {
                    (StatusCode::BAD_REQUEST, "invalid gzip request body").into_response()
                }
                GzipDecodeError::TooLarge => payload_too_large(),
            })
        }),
        BodyEncoding::Identity if body.len() > MAX_FETCH_REQUEST_BODY => {
            Err(Box::new(payload_too_large()))
        }
        BodyEncoding::Identity => Ok(body.to_vec()),
    }
}

/// Buffer a fetch request body, refusing anything over [`MAX_FETCH_REQUEST_BODY`].
async fn read_fetch_request_body(body: Body) -> Result<Bytes, Box<Response<Body>>> {
    axum::body::to_bytes(body, MAX_FETCH_REQUEST_BODY)
        .await
        .map_err(|e| {
            let inner = e.into_inner();
            let too_large =
                std::iter::successors(Some(&*inner as &(dyn std::error::Error + 'static)), |e| {
                    e.source()
                })
                .any(|e| e.is::<http_body_util::LengthLimitError>());
            Box::new(if too_large {
                payload_too_large()
            } else {
                (StatusCode::BAD_REQUEST, "could not read request body").into_response()
            })
        })
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

/// Decoded bytes a streamed gzip body may always expand to, matching the fetch cap.
const GZIP_STREAM_FREE_EXPANSION: u64 = MAX_FETCH_REQUEST_BODY as u64;

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

    /// Decode a buffered body the way [`rpc`] does for fetch requests.
    fn decode_body(headers: &HeaderMap, body: &[u8]) -> Result<Vec<u8>, Box<Response<Body>>> {
        decode_fetch_request_body(body_encoding(headers)?, body)
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
        let out = match decode_body(&headers, &gzip) {
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
        match decode_body(&headers, &gzip) {
            Err(resp) => assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE),
            Ok(_) => panic!("oversized gzip must fail"),
        }
    }

    #[test]
    fn identity_body_over_limit_returns_payload_too_large() {
        let body = vec![0u8; MAX_FETCH_REQUEST_BODY + 1];
        let headers = HeaderMap::new();
        match decode_body(&headers, &body) {
            Err(resp) => assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE),
            Ok(_) => panic!("oversized body must fail"),
        }
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
