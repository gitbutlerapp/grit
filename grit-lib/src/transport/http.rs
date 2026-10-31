//! Smart-HTTP Git transport over a pluggable HTTP client.
//!
//! This module ports the smart-HTTP fetch protocol from the CLI's
//! `http_smart.rs` into an embedder-shaped surface:
//!
//! * [`HttpClient`] — the minimal request surface the protocol needs: a `GET`
//!   (used for `info/refs?service=git-upload-pack` discovery) and a `POST`
//!   (used for the stateless-RPC `git-upload-pack` / `git-receive-pack`
//!   request body). Embedders supply their own client so grit-lib never forces
//!   a particular TLS / async / proxy stack on them.
//! * [`SmartHttpTransport`] — a [`Transport`] that performs the `info/refs`
//!   discovery on [`Transport::connect`] and exposes the parsed advertisement
//!   through a [`Connection`].
//! * [`http_fetch`] — drives the stateless-RPC negotiation (`want`/`have`/`done`
//!   over repeated POSTs), demultiplexes the side-band pack, ingests it, and
//!   returns a fetch outcome using the same refspec/tag/prune helpers as other
//!   fetch paths.
//!
//! A default `ureq`-backed [`HttpClient`] lives in the `transport::http::ureq_client`
//! module behind the `http-ureq` cargo feature; it wires a
//! [`CredentialProvider`](crate::credentials::CredentialProvider) for HTTP basic auth on `401`.
//!
//! Both protocol v0/v1 (the classic stateless RPC) and protocol v2 (the
//! stateless multi-POST flow) are implemented here. A v2 server is detected from
//! the `version 2` capability advertisement returned by `info/refs` (requested
//! with the `Git-Protocol: version=2` header); [`http_fetch`] then runs the v2
//! `command=ls-refs` + `command=fetch` rounds as separate POSTs — each round
//! resends the capability echo, all `want`s, and the accumulated `have`s —
//! reusing the shared v2 request framing and side-band demuxer from the fetch module.

use std::collections::HashSet;
use std::io::{Cursor, Read};
use std::path::Path;

use crate::error::{Error, RemoteUnreadableReason, Result};
use crate::fetch::Progress;
use crate::objects::ObjectId;
use crate::pkt_line;
use crate::transfer::{FetchOptions, FetchOutcome};
use crate::transport::stateless_http::StatelessHttpConnection;
use crate::transport::{ConnectOptions, Connection, Service, Transport};

#[cfg(feature = "http-ureq")]
pub mod ureq_client;

/// The minimal HTTP surface the smart-HTTP transport needs.
///
/// Implementations legitimately perform real network I/O; the trait makes no
/// assumption about the underlying stack (blocking/async, TLS provider, proxy,
/// cookies), so an embedder can route Git's HTTP through whatever client it
/// already uses.
///
/// The `git_protocol` argument carries the value of the `Git-Protocol` request
/// header (e.g. `version=2`) when the caller wants to negotiate a protocol
/// version; pass it through verbatim. A default `Git-Protocol` for every request
/// may be supplied via [`HttpClient::git_protocol_header`].
pub trait HttpClient: Send + Sync {
    /// Issue a `GET` to `url`, returning the response body bytes.
    ///
    /// # Errors
    ///
    /// Returns an error on a transport failure or a non-success HTTP status.
    fn get(&self, url: &str, git_protocol: Option<&str>) -> Result<Vec<u8>>;

    /// Issue a `POST` to `url` with the given `content_type`, `accept` header,
    /// and request `body`, returning the response body bytes.
    ///
    /// # Errors
    ///
    /// Returns an error on a transport failure or a non-success HTTP status.
    fn post(
        &self,
        url: &str,
        content_type: &str,
        accept: &str,
        body: &[u8],
        git_protocol: Option<&str>,
    ) -> Result<Vec<u8>>;

    /// Issue a `POST` and return the response body as a readable stream.
    ///
    /// The default implementation buffers via [`post`](Self::post). Embedders
    /// should override this when large packfiles make buffering untenable.
    ///
    /// # Errors
    ///
    /// Same as [`post`](Self::post).
    fn post_into_reader(
        &self,
        url: &str,
        content_type: &str,
        accept: &str,
        body: &[u8],
        git_protocol: Option<&str>,
    ) -> Result<Box<dyn Read + Send>> {
        Ok(Box::new(Cursor::new(self.post(
            url,
            content_type,
            accept,
            body,
            git_protocol,
        )?)))
    }

    /// Issue a `POST` whose body is read from `body` while it is sent, and return
    /// the response body as a readable stream.
    ///
    /// Push uses this for request bodies too large to hold in memory (the
    /// packfile), sending them with chunked transfer encoding. The body cannot be
    /// replayed, so the client should not expect an authentication challenge
    /// here: the caller posts a small probe request through
    /// [`post_into_reader`](Self::post_into_reader) first, which settles
    /// credentials the way Git does before a large push.
    ///
    /// The default implementation reads `body` into memory and calls
    /// [`post_into_reader`](Self::post_into_reader). Embedders should override it
    /// so large pushes are not buffered.
    ///
    /// # Errors
    ///
    /// Returns an error on a transport failure, a failure reading `body`, or a
    /// non-success HTTP status (including `401`, since the body cannot be resent).
    fn post_streaming(
        &self,
        url: &str,
        content_type: &str,
        accept: &str,
        body: &mut dyn Read,
        git_protocol: Option<&str>,
    ) -> Result<Box<dyn Read + Send>> {
        let mut buffered = Vec::new();
        body.read_to_end(&mut buffered).map_err(Error::Io)?;
        self.post_into_reader(url, content_type, accept, &buffered, git_protocol)
    }

    /// Issue a `GET` to `url` and return both the response body and the final
    /// URL the request resolved to after any HTTP redirects the client followed
    /// (`None` when the client does not track it).
    ///
    /// The smart-HTTP transport uses this on the `info/refs` discovery GET to
    /// re-base subsequent `git-upload-pack` POSTs onto a redirected location
    /// (Git's `http.followRedirects`): a host that redirects `info/refs` to a
    /// backing host expects the stateless-RPC POSTs there too, and many HTTP
    /// clients follow redirects on GET but not on POST. The default
    /// implementation calls [`get`](Self::get) and reports no final URL, so
    /// existing clients keep working unchanged (without redirect re-basing).
    ///
    /// # Errors
    ///
    /// Returns an error on a transport failure or a non-success HTTP status.
    fn get_with_final_url(
        &self,
        url: &str,
        git_protocol: Option<&str>,
    ) -> Result<(Vec<u8>, Option<String>)> {
        Ok((self.get(url, git_protocol)?, None))
    }

    /// Like [`Self::get_with_final_url`], but does not merge [`Self::git_protocol_header`]
    /// when `git_protocol` is `None` (used for forced v0/v1 `list_refs`).
    fn get_with_final_url_exact(
        &self,
        url: &str,
        git_protocol: Option<&str>,
    ) -> Result<(Vec<u8>, Option<String>)> {
        self.get_with_final_url(url, git_protocol)
    }

    /// The default `Git-Protocol` request-header value to apply when the caller
    /// passes `None`. Defaults to no header.
    fn git_protocol_header(&self) -> Option<&str> {
        None
    }

    /// Whether smart-HTTP is enabled (vs. dumb-HTTP fallback). Defaults to
    /// `true`; embedders that honor `GIT_SMART_HTTP=0` may return `false`.
    fn smart_http_enabled(&self) -> bool {
        true
    }

    /// Drop cached HTTP authorization after an `info/refs` redirect changed the
    /// effective repository base (Git re-binds credentials via `credential_from_url`).
    fn reset_auth_after_redirect_rebase(&self) {}

    /// Send an arbitrary request and return the response without judging its
    /// status.
    ///
    /// The resumable transfers ([`crate::remote::resumable`]) need methods and headers
    /// the smart-HTTP calls above do not: `HEAD`, `PATCH`, `Range` and the TUS
    /// headers. Unlike [`Self::get`] and [`Self::post`], every status comes back
    /// as `Ok` (a `206`, `404` or `409` means something to those callers); only
    /// a transport failure, or an authentication failure the client could not
    /// resolve, is an error. Clients that authenticate should do so here as
    /// they do for [`Self::get`].
    ///
    /// The default implementation reports [`Error::Message`] saying the client
    /// cannot send such requests, which turns resumable transfers off.
    ///
    /// # Errors
    ///
    /// A transport or authentication failure.
    fn send(&self, request: &HttpRequest<'_>) -> Result<HttpResponse> {
        Err(Error::Message(format!(
            "this HTTP client cannot send {} requests",
            request.method.as_str()
        )))
    }
}

/// Method of an [`HttpRequest`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HttpMethod {
    /// `GET`.
    Get,
    /// `HEAD`.
    Head,
    /// `POST`.
    Post,
    /// `PATCH`.
    Patch,
    /// `PUT`.
    Put,
    /// `DELETE`.
    Delete,
}

impl HttpMethod {
    /// The method name on the wire.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Head => "HEAD",
            Self::Post => "POST",
            Self::Patch => "PATCH",
            Self::Put => "PUT",
            Self::Delete => "DELETE",
        }
    }
}

/// A request for [`HttpClient::send`].
#[derive(Debug, Clone)]
pub struct HttpRequest<'a> {
    /// Request method.
    pub method: HttpMethod,
    /// Absolute URL.
    pub url: &'a str,
    /// Extra request headers, in order.
    pub headers: Vec<(&'static str, String)>,
    /// Request body; empty for `GET` and `HEAD`.
    pub body: &'a [u8],
    /// Whether the request may run longer than the client's usual whole-request
    /// timeout (a large download or upload).
    pub long_running: bool,
}

impl<'a> HttpRequest<'a> {
    /// A request with no headers and no body.
    #[must_use]
    pub fn new(method: HttpMethod, url: &'a str) -> Self {
        Self {
            method,
            url,
            headers: Vec::new(),
            body: &[],
            long_running: false,
        }
    }

    /// Add a header.
    #[must_use]
    pub fn header(mut self, name: &'static str, value: impl Into<String>) -> Self {
        self.headers.push((name, value.into()));
        self
    }

    /// Set the body.
    #[must_use]
    pub fn body(mut self, body: &'a [u8]) -> Self {
        self.body = body;
        self
    }

    /// Mark the request as a long transfer (see [`Self::long_running`]).
    #[must_use]
    pub fn long_running(mut self) -> Self {
        self.long_running = true;
        self
    }
}

/// A response from [`HttpClient::send`].
pub struct HttpResponse {
    /// Status code.
    pub status: u16,
    /// Response headers; names are lowercase.
    pub headers: Vec<(String, String)>,
    /// The body, read as it arrives.
    pub body: Box<dyn Read + Send>,
}

impl HttpResponse {
    /// The first value of header `name` (case-insensitive).
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// Whether the status is in the 2xx range.
    #[must_use]
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// Read the whole body.
    ///
    /// # Errors
    ///
    /// The read error.
    pub fn into_bytes(mut self) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        self.body.read_to_end(&mut out).map_err(Error::Io)?;
        Ok(out)
    }
}

impl std::fmt::Debug for HttpResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpResponse")
            .field("status", &self.status)
            .field("headers", &self.headers)
            .finish_non_exhaustive()
    }
}

/// Forward [`HttpClient`] through a shared [`std::sync::Arc`], so one client can
/// back several transports (and be observed by the caller) without moving it.
impl HttpClient for std::sync::Arc<dyn HttpClient> {
    fn get(&self, url: &str, git_protocol: Option<&str>) -> Result<Vec<u8>> {
        (**self).get(url, git_protocol)
    }

    fn post(
        &self,
        url: &str,
        content_type: &str,
        accept: &str,
        body: &[u8],
        git_protocol: Option<&str>,
    ) -> Result<Vec<u8>> {
        (**self).post(url, content_type, accept, body, git_protocol)
    }

    fn post_into_reader(
        &self,
        url: &str,
        content_type: &str,
        accept: &str,
        body: &[u8],
        git_protocol: Option<&str>,
    ) -> Result<Box<dyn Read + Send>> {
        (**self).post_into_reader(url, content_type, accept, body, git_protocol)
    }

    fn post_streaming(
        &self,
        url: &str,
        content_type: &str,
        accept: &str,
        body: &mut dyn Read,
        git_protocol: Option<&str>,
    ) -> Result<Box<dyn Read + Send>> {
        (**self).post_streaming(url, content_type, accept, body, git_protocol)
    }

    fn get_with_final_url(
        &self,
        url: &str,
        git_protocol: Option<&str>,
    ) -> Result<(Vec<u8>, Option<String>)> {
        (**self).get_with_final_url(url, git_protocol)
    }

    fn get_with_final_url_exact(
        &self,
        url: &str,
        git_protocol: Option<&str>,
    ) -> Result<(Vec<u8>, Option<String>)> {
        (**self).get_with_final_url_exact(url, git_protocol)
    }

    fn git_protocol_header(&self) -> Option<&str> {
        (**self).git_protocol_header()
    }

    fn smart_http_enabled(&self) -> bool {
        (**self).smart_http_enabled()
    }

    fn reset_auth_after_redirect_rebase(&self) {
        (**self).reset_auth_after_redirect_rebase();
    }

    fn send(&self, request: &HttpRequest<'_>) -> Result<HttpResponse> {
        (**self).send(request)
    }
}

impl<C: HttpClient> HttpClient for std::sync::Arc<C> {
    fn get(&self, url: &str, git_protocol: Option<&str>) -> Result<Vec<u8>> {
        (**self).get(url, git_protocol)
    }

    fn post(
        &self,
        url: &str,
        content_type: &str,
        accept: &str,
        body: &[u8],
        git_protocol: Option<&str>,
    ) -> Result<Vec<u8>> {
        (**self).post(url, content_type, accept, body, git_protocol)
    }

    fn post_into_reader(
        &self,
        url: &str,
        content_type: &str,
        accept: &str,
        body: &[u8],
        git_protocol: Option<&str>,
    ) -> Result<Box<dyn Read + Send>> {
        (**self).post_into_reader(url, content_type, accept, body, git_protocol)
    }

    fn post_streaming(
        &self,
        url: &str,
        content_type: &str,
        accept: &str,
        body: &mut dyn Read,
        git_protocol: Option<&str>,
    ) -> Result<Box<dyn Read + Send>> {
        (**self).post_streaming(url, content_type, accept, body, git_protocol)
    }

    fn get_with_final_url(
        &self,
        url: &str,
        git_protocol: Option<&str>,
    ) -> Result<(Vec<u8>, Option<String>)> {
        (**self).get_with_final_url(url, git_protocol)
    }

    fn get_with_final_url_exact(
        &self,
        url: &str,
        git_protocol: Option<&str>,
    ) -> Result<(Vec<u8>, Option<String>)> {
        (**self).get_with_final_url_exact(url, git_protocol)
    }

    fn git_protocol_header(&self) -> Option<&str> {
        (**self).git_protocol_header()
    }

    fn smart_http_enabled(&self) -> bool {
        (**self).smart_http_enabled()
    }

    fn reset_auth_after_redirect_rebase(&self) {
        (**self).reset_auth_after_redirect_rebase();
    }

    fn send(&self, request: &HttpRequest<'_>) -> Result<HttpResponse> {
        (**self).send(request)
    }
}

const UPLOAD_PACK: &str = "git-upload-pack";

/// HTTP metadata for an `info/refs` discovery response (when the transport captured it).
#[derive(Clone, Debug)]
pub(crate) struct InfoRefsDiscoveryHttp {
    pub url: String,
    pub status: u16,
    pub content_type: Option<String>,
}

/// Whether `url` is a smart-HTTP `info/refs` discovery GET.
#[cfg(feature = "http-ureq")]
#[must_use]
pub(crate) fn is_info_refs_discovery_url(url: &str) -> bool {
    url.contains("/info/refs") && url.contains("service=git-")
}

/// Keep only the media type from an HTTP `Content-Type` value (drop parameters such as `charset`).
#[must_use]
pub(crate) fn http_content_type_media_type(raw: &str) -> String {
    raw.split(';').next().unwrap_or(raw).trim().to_owned()
}

/// True when the response body or `Content-Type` is obviously not a Git advertisement.
#[must_use]
pub(crate) fn body_looks_like_non_git_server(body: &[u8], content_type: Option<&str>) -> bool {
    if content_type
        .is_some_and(|ct| http_content_type_media_type(ct).eq_ignore_ascii_case("text/html"))
    {
        return true;
    }
    let body = body.strip_prefix(b"\xef\xbb\xbf").unwrap_or(body);
    body.starts_with(b"<!")
        || body
            .get(..4)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case(b"<htm"))
}

pub(crate) fn not_git_server_error(
    url: Option<String>,
    status: u16,
    content_type: Option<String>,
) -> Error {
    Error::RemoteUnreadable {
        url,
        reason: RemoteUnreadableReason::NotGitServer {
            status,
            content_type,
        },
    }
}

fn reject_non_git_discovery_http(http: &InfoRefsDiscoveryHttp, body: &[u8]) -> Result<()> {
    if body_looks_like_non_git_server(body, http.content_type.as_deref()) {
        return Err(not_git_server_error(
            Some(http.url.clone()),
            http.status,
            http.content_type.clone(),
        ));
    }
    Ok(())
}

fn strip_service_advertisement_for_discovery<'a>(
    body: &'a [u8],
    http: Option<&InfoRefsDiscoveryHttp>,
) -> Result<&'a [u8]> {
    if let Some(ctx) = http {
        reject_non_git_discovery_http(ctx, body)?;
    } else if body_looks_like_non_git_server(body, None) {
        return Err(not_git_server_error(None, 200, None));
    }
    match strip_service_advertisement(body) {
        Ok(stripped) => Ok(stripped),
        Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::InvalidData => {
            if let Some(ctx) = http {
                Err(not_git_server_error(
                    Some(ctx.url.clone()),
                    ctx.status,
                    ctx.content_type.clone(),
                ))
            } else {
                Err(not_git_server_error(None, 200, None))
            }
        }
        Err(e) => Err(e),
    }
}

/// Strip the optional `# service=...\n` pkt-line + flush preamble that a
/// smart-HTTP `info/refs?service=...` response begins with, returning the
/// remaining advertisement bytes.
///
/// A smart server prefixes the advertisement with `001e# service=git-upload-pack\n`
/// followed by a `0000` flush; a dumb server (or a raw `upload-pack
/// --advertise-refs` body) omits it. Lifted from the CLI's
/// `strip_v0_service_advertisement_if_present`.
fn strip_service_advertisement(body: &[u8]) -> Result<&[u8]> {
    let mut cur = Cursor::new(body);
    let start = cur.position();
    match pkt_line::read_packet(&mut cur)? {
        Some(pkt_line::Packet::Data(line)) if line.starts_with("# service=") => {
            // Consume the trailing flush after the service header.
            match pkt_line::read_packet(&mut cur)? {
                Some(pkt_line::Packet::Flush) | None => {}
                _ => {
                    // No flush after the service line: not a smart preamble; rewind.
                    return Ok(body);
                }
            }
            let pos = cur.position() as usize;
            Ok(&body[pos..])
        }
        _ => {
            cur.set_position(start);
            Ok(body)
        }
    }
}

/// A parsed v0/v1 advertisement ref entry (name -> oid).
#[derive(Clone, Debug)]
struct AdvRef {
    name: String,
    oid: ObjectId,
}

/// The discovery outcome: protocol version, advertised refs, capabilities, and
/// the symref target for `HEAD` (if any).
struct Discovery {
    protocol_version: u8,
    refs: Vec<AdvRef>,
    caps: HashSet<String>,
    head_symref: Option<String>,
    object_format: String,
}

/// Parse a v0/v1 ref advertisement (after the service preamble is stripped).
///
/// Hash-width aware via [`ObjectId::from_hex`]. Capabilities ride on the NUL
/// suffix of the first ref line; the `symref=HEAD:<target>` capability records
/// the default branch. The all-zero "unborn HEAD" carrier and `shallow`
/// trailers are skipped. Lifted from the CLI's `parse_v0_v1_advertisement` /
/// `discover_http_protocol`.
fn parse_advertisement(body: &[u8]) -> Result<Discovery> {
    let mut cur = Cursor::new(body);

    // Peek the first packet to distinguish v2 from v0/v1.
    let first = match pkt_line::read_packet(&mut cur)? {
        None | Some(pkt_line::Packet::Flush) => {
            // Empty advertisement (empty repo on an older server): no refs.
            return Ok(Discovery {
                protocol_version: 0,
                refs: Vec::new(),
                caps: HashSet::new(),
                head_symref: None,
                object_format: "sha1".to_owned(),
            });
        }
        Some(pkt_line::Packet::Data(s)) => s,
        Some(other) => {
            return Err(Error::Message(format!(
                "unexpected first advertisement packet: {other:?}"
            )))
        }
    };
    pkt_line::reject_err_line(&first)?;
    if first.trim_end() == "version 2" {
        // Detect v2 so the caller can report it as unsupported in this pass.
        let mut caps = HashSet::new();
        loop {
            match pkt_line::read_packet(&mut cur)? {
                None | Some(pkt_line::Packet::Flush) => break,
                Some(pkt_line::Packet::Data(s)) => {
                    pkt_line::reject_err_line(&s)?;
                    caps.insert(s.trim_end().to_owned());
                }
                Some(_) => break,
            }
        }
        let object_format = caps
            .iter()
            .find_map(|c| c.strip_prefix("object-format="))
            .unwrap_or("sha1")
            .to_owned();
        return Ok(Discovery {
            protocol_version: 2,
            refs: Vec::new(),
            caps,
            head_symref: None,
            object_format,
        });
    }

    // v0/v1: rewind and parse the ref lines.
    cur.set_position(0);
    let mut refs = Vec::new();
    let mut caps: HashSet<String> = HashSet::new();
    let mut head_symref = None;
    let mut first_ref_line = true;
    loop {
        match pkt_line::read_packet(&mut cur)? {
            None | Some(pkt_line::Packet::Flush) => break,
            Some(pkt_line::Packet::Data(line)) => {
                let line = line.trim_end_matches('\n');
                pkt_line::reject_err_line(line)?;
                if line.starts_with("version ") {
                    continue;
                }
                if line.starts_with("shallow ") || line.starts_with("unshallow ") {
                    continue;
                }
                let (payload, cap_part) = match line.split_once('\0') {
                    Some((p, c)) => (p.trim(), Some(c)),
                    None => (line.trim(), None),
                };
                let Some((oid_hex, refname)) =
                    payload.split_once('\t').or_else(|| payload.split_once(' '))
                else {
                    continue;
                };
                let oid_hex = oid_hex.trim();
                let refname = refname.trim();
                if first_ref_line {
                    if let Some(raw_caps) = cap_part {
                        for cap in raw_caps.split_whitespace() {
                            if let Some(target) = cap.strip_prefix("symref=HEAD:") {
                                if crate::refs::is_valid_advertised_symref_target(target) {
                                    head_symref = Some(target.to_owned());
                                }
                            }
                            caps.insert(cap.to_owned());
                        }
                    }
                    first_ref_line = false;
                }
                if refname.is_empty() {
                    continue;
                }
                // All-zero OID marks the unborn-HEAD capabilities carrier (empty repo).
                if oid_hex.bytes().all(|b| b == b'0') {
                    continue;
                }
                let oid = ObjectId::from_hex(oid_hex).map_err(|e| {
                    Error::Message(format!("bad oid in advertisement: {oid_hex}: {e}"))
                })?;
                let valid_ref = refname == "HEAD"
                    || crate::refs::is_valid_fetch_advertised_ref(refname)
                    || refname
                        .strip_suffix("^{}")
                        .is_some_and(crate::refs::is_valid_fetch_advertised_ref);
                if valid_ref {
                    refs.push(AdvRef {
                        name: refname.to_owned(),
                        oid,
                    });
                }
            }
            Some(other) => {
                return Err(Error::Message(format!(
                    "unexpected packet in advertisement: {other:?}"
                )))
            }
        }
    }
    let object_format = caps
        .iter()
        .find_map(|c| c.strip_prefix("object-format="))
        .unwrap_or("sha1")
        .to_owned();
    Ok(Discovery {
        protocol_version: if caps.contains("version 1") { 1 } else { 0 },
        refs,
        caps,
        head_symref,
        object_format,
    })
}

/// Build the `info/refs?service=git-upload-pack` discovery URL for `repo_url`.
fn info_refs_url(repo_url: &str) -> String {
    smart_info_refs_discovery_url(repo_url)
}

/// Parsed smart-HTTP upload-pack discovery: protocol version, refs, caps, HEAD symref.
pub type UploadPackDiscovery = (u8, Vec<(String, ObjectId)>, Vec<String>, Option<String>);

fn upload_pack_discovery_from_body(body: &[u8]) -> Result<UploadPackDiscovery> {
    upload_pack_discovery_from_body_with_http(body, None)
}

fn upload_pack_discovery_from_body_with_http(
    body: &[u8],
    http: Option<&InfoRefsDiscoveryHttp>,
) -> Result<UploadPackDiscovery> {
    let stripped = strip_service_advertisement_for_discovery(body, http)?;
    let disc = parse_advertisement(stripped)?;
    let refs: Vec<(String, ObjectId)> = disc
        .refs
        .iter()
        .filter(|r| r.name != "HEAD" && !r.name.ends_with("^{}"))
        .map(|r| (r.name.clone(), r.oid))
        .collect();
    let caps: Vec<String> = disc.caps.iter().cloned().collect();
    Ok((disc.protocol_version, refs, caps, disc.head_symref))
}

/// Parse a smart-HTTP `info/refs` response body for `git-upload-pack`.
///
/// # Errors
///
/// [`Error::RemoteFatal`] when the server answers with an `ERR` packet instead of an
/// advertisement; other malformed advertisements fail with parse errors.
pub fn discover_upload_pack_from_body(body: &[u8]) -> Result<UploadPackDiscovery> {
    upload_pack_discovery_from_body(body)
}

/// Like [`discover_upload_pack_from_body`], but retains annotated-tag `^{}` peel lines
/// (for `git ls-remote`-style ref listing).
pub fn discover_upload_pack_for_list_refs_from_body(body: &[u8]) -> Result<UploadPackDiscovery> {
    let stripped = strip_service_advertisement_for_discovery(body, None)?;
    let disc = parse_advertisement(stripped)?;
    let refs: Vec<(String, ObjectId)> = disc
        .refs
        .iter()
        .filter(|r| r.name != "HEAD")
        .map(|r| (r.name.clone(), r.oid))
        .collect();
    let caps: Vec<String> = disc.caps.iter().cloned().collect();
    Ok((disc.protocol_version, refs, caps, disc.head_symref))
}

/// Discover upload-pack for ref listing (peel lines preserved).
pub fn discover_upload_pack_for_list_refs(
    client: &dyn HttpClient,
    repo_url: &str,
    git_protocol: Option<&str>,
) -> Result<UploadPackDiscovery> {
    let url = info_refs_url(repo_url);
    let (body, _) = client.get_with_final_url_exact(&url, git_protocol)?;
    discover_upload_pack_for_list_refs_from_body(&body)
}

/// Discover `git-upload-pack` capabilities and (for v0/v1) advertised refs.
///
/// For protocol v2 the ref list is empty; use `command=ls-refs` to enumerate refs.
pub fn discover_upload_pack(
    client: &dyn HttpClient,
    repo_url: &str,
    git_protocol: Option<&str>,
) -> Result<UploadPackDiscovery> {
    let url = info_refs_url(repo_url);
    let gp = git_protocol.or_else(|| client.git_protocol_header());
    let body = client.get(&url, gp)?;
    upload_pack_discovery_from_body(&body)
}

/// Discover upload-pack using an exact `Git-Protocol` header (no client default).
pub fn discover_upload_pack_exact(
    client: &dyn HttpClient,
    repo_url: &str,
    git_protocol: Option<&str>,
) -> Result<UploadPackDiscovery> {
    let url = info_refs_url(repo_url);
    let (body, _) = client.get_with_final_url_exact(&url, git_protocol)?;
    upload_pack_discovery_from_body(&body)
}

/// Build the smart-HTTP `info/refs?service=git-upload-pack` discovery URL for a repo base.
#[must_use]
pub fn smart_info_refs_discovery_url(repo_base: &str) -> String {
    let base = repo_base.trim_end_matches('/');
    let mut url = format!("{base}/info/refs");
    url.push_str(if url.contains('?') { "&" } else { "?" });
    url.push_str("service=");
    url.push_str(UPLOAD_PACK);
    url
}

/// Strip the `/info/refs` suffix (and query) from a discovery URL, returning the repo base.
#[must_use]
pub fn repo_base_from_info_refs_url(info_refs_url: &str) -> Option<String> {
    let path = info_refs_url.split('?').next()?;
    let base = path.strip_suffix("/info/refs")?.trim_end_matches('/');
    (!base.is_empty()).then(|| base.to_string())
}

/// After an `info/refs` GET that returned `401` following redirects, return the
/// discovery URL whose path matches the effective repository base (Git's
/// `credential_from_url` after `update_url_from_redirect`).
#[must_use]
pub fn effective_info_refs_url_after_redirect(
    requested_info_refs_url: &str,
    final_url: Option<&str>,
) -> String {
    let Some(original_base) = repo_base_from_info_refs_url(requested_info_refs_url) else {
        return requested_info_refs_url.to_string();
    };
    match rebased_base_from_redirect(&original_base, final_url) {
        Some(new_base) => smart_info_refs_discovery_url(&new_base),
        None => requested_info_refs_url.to_string(),
    }
}

/// Given the `original_base` of an `info/refs` discovery request and the final
/// URL it resolved to after any HTTP redirects the client followed, return the
/// re-based repo URL to use for subsequent smart-HTTP requests — or `None` when
/// there was no usable redirect (keep the original base).
///
/// This implements the client side of Git's `http.followRedirects`: when a host
/// redirects `info/refs` to a backing location (e.g. tangled.org → its "knot"
/// host), the later `git-upload-pack` POSTs must target the redirected location.
/// Many HTTP clients follow the redirect on the discovery GET but not on a POST
/// (which then hits the redirecting host and comes back as an un-followed `3xx`
/// with an empty body — zero refs, a silently empty clone). The redirect must
/// preserve the `/info/refs` path suffix; the new base is the final URL with
/// that suffix (and any query) removed.
#[must_use]
pub fn rebased_base_from_redirect(original_base: &str, final_url: Option<&str>) -> Option<String> {
    let final_url = final_url?;
    let final_path = final_url.split('?').next().unwrap_or(final_url);
    let new_base = final_path.strip_suffix("/info/refs")?.trim_end_matches('/');
    if new_base.is_empty() || new_base == original_base.trim_end_matches('/') {
        return None;
    }
    Some(new_base.to_owned())
}

/// Normalized HTTP origin (`scheme://host:port`) used to scope cached credentials
/// and cookies across redirects.
///
/// Host names are lowercased and not aliased (`127.0.0.1` and `localhost` stay
/// distinct). Non-default ports are always included; default ports for the scheme
/// are filled in when omitted from the URL.
#[must_use]
pub fn http_origin_key(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    let scheme = parsed.scheme().to_ascii_lowercase();
    let host = parsed.host_str()?.to_ascii_lowercase();
    let port = parsed.port_or_known_default()?;
    Some(format!("{scheme}://{host}:{port}"))
}

/// Returns whether two URLs share the same HTTP origin (scheme, host, port).
#[must_use]
pub fn http_origins_match(a: &str, b: &str) -> bool {
    match (http_origin_key(a), http_origin_key(b)) {
        (Some(left), Some(right)) => left == right,
        _ => false,
    }
}

/// A smart-HTTP [`Transport`] over a pluggable [`HttpClient`].
///
/// [`Transport::connect`] performs the `info/refs?service=git-upload-pack`
/// discovery GET and parses the advertisement; the returned [`Connection`]
/// exposes the advertised refs/capabilities. Use [`http_fetch`] to drive the
/// fetch negotiation over the same client.
pub struct SmartHttpTransport<C: HttpClient> {
    client: C,
}

impl<C: HttpClient + Clone + 'static> SmartHttpTransport<C> {
    /// Build a transport backed by `client`.
    pub fn new(client: C) -> Self {
        Self { client }
    }

    /// Borrow the underlying HTTP client.
    pub fn client(&self) -> &C {
        &self.client
    }

    /// Push `refs` to `repo_url` over smart HTTP (`git-receive-pack`), returning a
    /// [`crate::transfer::PushOutcome`].
    ///
    /// This is the push counterpart to [`http_fetch`]: it discovers the
    /// receive-pack advertisement, decides each update, builds the command block +
    /// pack, POSTs `git-receive-pack`, and parses the `report-status` reply —
    /// reusing the same decision/pack/report machinery as the duplex
    /// [`crate::push::push_remote`]. Delegates to [`crate::push::push_http`].
    ///
    /// Protocol v0/v1 only (a v2 receive-pack advertisement is rejected).
    ///
    /// # Errors
    ///
    /// Returns an error if discovery fails, the advertisement is protocol v2, a
    /// source object is missing locally, the pack build fails, or on wire/parse
    /// I/O failure.
    pub fn push(
        &self,
        local_git_dir: &Path,
        repo_url: &str,
        refs: &[crate::transfer::PushRefSpec],
        opts: &crate::transfer::PushOptions,
        progress: &mut dyn Progress,
        cancel: &dyn crate::progress::Cancel,
    ) -> Result<crate::transfer::PushOutcome> {
        use crate::push::push_remote;
        use crate::transport::{ConnectOptions, Transport};
        let connect = ConnectOptions {
            protocol_version: 0,
            diagnostics: opts.diagnostics.clone(),
            network_trace: opts.network_trace,
            ..Default::default()
        };
        let mut conn = self.connect(repo_url, Service::ReceivePack, &connect)?;
        push_remote(local_git_dir, &mut *conn, refs, opts, progress, cancel)
    }

    /// Perform the `info/refs` discovery for `repo_url` and `service`, returning
    /// the parsed [`Discovery`].
    ///
    /// `git_protocol` is the `Git-Protocol` request-header value to apply (e.g.
    /// `version=2` to request a v2 advertisement); when `None`, the client's
    /// default ([`HttpClient::git_protocol_header`]) is used.
    /// Discover capabilities/refs and re-base the repo URL when `info/refs` redirects.
    fn discover_with_redirect(
        &self,
        repo_url: &str,
        service: Service,
        git_protocol: Option<&str>,
    ) -> Result<(String, Discovery)> {
        let info_url = info_refs_url_for_service(repo_url, service);
        let gp = git_protocol.or_else(|| self.client.git_protocol_header());
        let (body, final_url) = self.client.get_with_final_url(&info_url, gp)?;
        if let Some(new_base) = rebased_base_from_redirect(repo_url, final_url.as_deref()) {
            let _redirect = format!("http(s): redirected base {repo_url} -> {new_base}");
            self.client.reset_auth_after_redirect_rebase();
            let url = info_refs_url_for_service(&new_base, service);
            let body = self.client.get(&url, gp)?;
            let stripped = strip_service_advertisement_for_discovery(&body, None)?;
            let disc = parse_advertisement(stripped)?;
            return Ok((new_base, disc));
        }
        let stripped = strip_service_advertisement_for_discovery(&body, None)?;
        let disc = parse_advertisement(stripped)?;
        Ok((repo_url.to_owned(), disc))
    }
}

fn info_refs_url_for_service(repo_url: &str, service: Service) -> String {
    let base = repo_url.trim_end_matches('/');
    let mut url = format!("{base}/info/refs");
    url.push_str(if url.contains('?') { "&" } else { "?" });
    url.push_str("service=");
    url.push_str(service.wire_name());
    url
}

/// The `Git-Protocol` request-header value for a requested protocol version, or
/// `None` for v0 (no header — the classic advertisement).
fn git_protocol_for_version(version: u8) -> Option<String> {
    if version >= 1 {
        Some(format!("version={version}"))
    } else {
        None
    }
}

impl<C: HttpClient + Clone + 'static> Transport for SmartHttpTransport<C> {
    fn connect(
        &self,
        url: &str,
        service: Service,
        opts: &ConnectOptions,
    ) -> Result<Box<dyn Connection>> {
        crate::net_trace::net_trace!(
            opts.network_trace,
            opts.diagnostics.as_ref(),
            "http(s) discover {url} (service={}, request protocol v{})",
            service.wire_name(),
            opts.protocol_version
        );
        let gp = git_protocol_for_version(opts.protocol_version);
        let (repo_url, disc) = self.discover_with_redirect(url, service, gp.as_deref())?;
        let adv_refs: Vec<(String, ObjectId)> = disc
            .refs
            .iter()
            .filter(|r| r.name != "HEAD" && !r.name.ends_with("^{}"))
            .map(|r| (r.name.clone(), r.oid))
            .collect();
        let caps: Vec<String> = disc.caps.iter().cloned().collect();
        crate::net_trace::net_trace!(
            opts.network_trace,
            opts.diagnostics.as_ref(),
            "http(s) discovered: protocol v{}, {} ref(s) advertised",
            disc.protocol_version,
            adv_refs.len()
        );
        Ok(Box::new(StatelessHttpConnection::new(
            self.client.clone(),
            service,
            repo_url,
            disc.protocol_version,
            adv_refs,
            caps,
            disc.head_symref,
            disc.object_format,
            gp,
        )))
    }
}

/// Wrap an owned HTTP client for [`http_fetch`] / [`SmartHttpTransport`].
pub fn http_client_arc(client: impl HttpClient + 'static) -> std::sync::Arc<dyn HttpClient> {
    let boxed: Box<dyn HttpClient> = Box::new(client);
    std::sync::Arc::from(boxed)
}

/// Fetch from a smart-HTTP remote via the shared [`crate::fetch::fetch_remote`] engine.
///
/// Performs `info/refs` discovery (with redirect re-basing) and drives stateless
/// `git-upload-pack` POSTs through a [`crate::transport::stateless_http::StatelessHttpConnection`].
///
/// # Errors
///
/// Returns an error if discovery, negotiation, pack ingest, or ref updates fail.
pub fn http_fetch(
    client: std::sync::Arc<dyn HttpClient>,
    local_git_dir: &Path,
    repo_url: &str,
    opts: &mut FetchOptions,
    progress: &mut dyn Progress,
) -> Result<FetchOutcome> {
    use crate::fetch::fetch_remote;
    use crate::transport::Transport;
    let protocol_version = client
        .git_protocol_header()
        .and_then(|h| h.strip_prefix("version="))
        .and_then(|v| v.parse::<u8>().ok())
        .unwrap_or(0);
    let connect = ConnectOptions {
        protocol_version,
        diagnostics: opts.diagnostics.clone(),
        network_trace: opts.network_trace,
        ..Default::default()
    };
    let transport = SmartHttpTransport::new(client);
    let mut conn = transport.connect(repo_url, Service::UploadPack, &connect)?;
    fetch_remote(
        local_git_dir,
        &mut *conn,
        opts,
        progress,
        &crate::progress::NeverCancel,
    )
}

#[cfg(test)]
#[path = "http_discovery_tests.rs"]
mod discovery_tests;
