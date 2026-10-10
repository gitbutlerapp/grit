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
//!   over repeated POSTs), demultiplexes the side-band pack, ingests it with
//!   [`crate::unpack_objects`], and returns a [`crate::transfer::FetchOutcome`]
//!   — reusing the same refspec/tag/prune/classification helpers as the
//!   in-process and `git://` fetch paths.
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
//! reusing the shared v2 request framing and side-band demuxer from
//! [`crate::fetch`].

use std::collections::HashSet;
use std::io::{Cursor, Read};
use std::path::Path;

use crate::error::{Error, Result};
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
}

const UPLOAD_PACK: &str = "git-upload-pack";

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
    if first.trim_end() == "version 2" {
        // Detect v2 so the caller can report it as unsupported in this pass.
        let mut caps = HashSet::new();
        loop {
            match pkt_line::read_packet(&mut cur)? {
                None | Some(pkt_line::Packet::Flush) => break,
                Some(pkt_line::Packet::Data(s)) => {
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
                if refname == "HEAD" || crate::refs::is_valid_fetch_advertised_ref(refname) {
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
    let stripped = strip_service_advertisement(body)?;
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
pub fn discover_upload_pack_from_body(body: &[u8]) -> Result<UploadPackDiscovery> {
    upload_pack_discovery_from_body(body)
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
        push_remote(local_git_dir, &mut *conn, refs, opts, progress)
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
            let disc = parse_advertisement(strip_service_advertisement(&body)?)?;
            return Ok((new_base, disc));
        }
        let disc = parse_advertisement(strip_service_advertisement(&body)?)?;
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

/// Read a length-prefixed pkt-line payload, returning `None` on flush/delim/EOF.
fn read_pkt_payload(r: &mut (impl Read + ?Sized)) -> std::io::Result<Option<Vec<u8>>> {
    let mut len_buf = [0u8; 4];
    match r.read_exact(&mut len_buf) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len_str = std::str::from_utf8(&len_buf)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let len = usize::from_str_radix(len_str, 16)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    match len {
        0..=2 => Ok(None),
        n if n <= 4 => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("invalid pkt-line length: {n}"),
        )),
        n => {
            let mut buf = vec![0u8; n - 4];
            r.read_exact(&mut buf)?;
            Ok(Some(buf))
        }
    }
}

/// Kind of `ACK` status suffix in a v0 negotiation response.
#[derive(Clone, Copy, PartialEq, Eq)]
enum AckKind {
    /// `ACK <oid>` with no status suffix (ends a round / post-`done`).
    Bare,
    /// `ACK <oid> common` — the server holds this commit; replay it on the next
    /// stateless RPC if we had not already marked it common.
    Common,
    /// `ACK <oid> continue` — recorded in the negotiator but not replayed.
    Continue,
    /// `ACK <oid> ready` — the server has enough; it will send the pack.
    Ready,
}

struct Ack {
    oid: ObjectId,
    kind: AckKind,
}

fn parse_ack(line: &str) -> Option<Ack> {
    let rest = line.strip_prefix("ACK ")?;
    let hex = rest.split_whitespace().next()?;
    let oid = ObjectId::from_hex(hex).ok()?;
    let tail = rest.strip_prefix(hex).unwrap_or("").trim();
    let kind = if tail.contains("continue") {
        AckKind::Continue
    } else if tail.contains("common") {
        AckKind::Common
    } else if tail.contains("ready") {
        AckKind::Ready
    } else {
        AckKind::Bare
    };
    Some(Ack { oid, kind })
}

/// Result of parsing one stateless-RPC response.
struct RoundResult {
    acks: Vec<Ack>,
    got_pack: bool,
    /// Shallow boundaries the server reported (`shallow <oid>`) in this response's
    /// leading `shallow-info` section (empty unless a deepen was requested).
    shallow: Vec<ObjectId>,
    /// Boundaries the server un-shallowed (`unshallow <oid>`) in this response.
    unshallow: Vec<ObjectId>,
}

/// Parse a stateless-RPC `git-upload-pack` response from a stream and write any
/// any pack bytes to `pack_receive` instead of a `Vec`.
fn read_stateless_response_stream<R: Read>(
    r: &mut R,
    sideband: bool,
    expect_shallow: bool,
    pack_receive: &mut crate::pack_receive::TempPackReceive,
    progress: &mut dyn Progress,
) -> Result<RoundResult> {
    let mut acks = Vec::new();
    let mut got_pack = false;
    let mut shallow = Vec::new();
    let mut unshallow = Vec::new();

    let mut replay_payload: Option<Vec<u8>> = None;
    let mut shallow_section_flush_skipped = false;
    if expect_shallow {
        loop {
            match pkt_line::read_packet(r)? {
                None | Some(pkt_line::Packet::Flush) => break,
                Some(pkt_line::Packet::Data(line)) => {
                    let trimmed = line.trim_end_matches('\n');
                    if let Some(rest) = trimmed.strip_prefix("shallow ") {
                        if let Ok(oid) = ObjectId::from_hex(rest.trim()) {
                            shallow.push(oid);
                        }
                    } else if let Some(rest) = trimmed.strip_prefix("unshallow ") {
                        if let Ok(oid) = ObjectId::from_hex(rest.trim()) {
                            unshallow.push(oid);
                        }
                    } else {
                        replay_payload = Some(line.into_bytes());
                        break;
                    }
                }
                Some(_) => break,
            }
        }
    }

    loop {
        let payload = if let Some(p) = replay_payload.take() {
            p
        } else {
            match read_pkt_payload(r)? {
                Some(p) => p,
                None => {
                    // v0 deepen: server may flush once between shallow lines and pack.
                    if expect_shallow && !got_pack && !shallow_section_flush_skipped {
                        shallow_section_flush_skipped = true;
                        continue;
                    }
                    break;
                }
            }
        };
        if payload.is_empty() {
            continue;
        }
        let is_pack =
            (sideband && payload.first() == Some(&1) && payload.get(1..5) == Some(b"PACK"))
                || payload.starts_with(b"PACK");
        if is_pack {
            got_pack = true;
            if sideband {
                let mut target =
                    crate::pack_receive::PackReceiveTarget::File(pack_receive.file_mut());
                let mut pending = Vec::new();
                let mut seen = false;
                if payload.first() == Some(&1) {
                    crate::pack_receive::append_pack_data(
                        &payload[1..],
                        &mut target,
                        &mut pending,
                        &mut seen,
                    )?;
                } else {
                    crate::pack_receive::append_pack_data(
                        &payload,
                        &mut target,
                        &mut pending,
                        &mut seen,
                    )?;
                }
                crate::pack_receive::read_sideband_pack_tail(
                    r,
                    &mut target,
                    progress,
                    &mut pending,
                    &mut seen,
                )?;
                pack_receive.mark_seen_pack(seen);
            } else {
                let data = if payload.starts_with(b"PACK") {
                    payload.as_slice()
                } else if let Some(pos) = payload.windows(4).position(|w| w == b"PACK") {
                    &payload[pos..]
                } else {
                    payload.as_slice()
                };
                pack_receive.write_raw(data)?;
                std::io::copy(r, pack_receive).map_err(Error::Io)?;
                pack_receive.mark_seen_pack(true);
            }
            break;
        }
        let text = String::from_utf8_lossy(&payload);
        let line = text.trim_end_matches('\n');
        if let Some(err) = line.strip_prefix("ERR ") {
            return Err(Error::Message(format!("remote upload-pack error: {err}")));
        }
        if line == "NAK" {
            continue;
        }
        if let Some(rest) = line.strip_prefix("shallow ") {
            if let Ok(oid) = ObjectId::from_hex(rest.trim()) {
                shallow.push(oid);
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix("unshallow ") {
            if let Ok(oid) = ObjectId::from_hex(rest.trim()) {
                unshallow.push(oid);
            }
            continue;
        }
        if let Some(ack) = parse_ack(line) {
            acks.push(ack);
        }
    }
    Ok(RoundResult {
        acks,
        got_pack,
        shallow,
        unshallow,
    })
}

/// The v0/v1 fetch capabilities we request, intersected with what the server
/// advertised. Mirrors `build_fetch_caps_v0`.
fn build_fetch_caps(caps: &HashSet<String>) -> String {
    let mut enabled = Vec::new();
    let multi_ack_detailed = caps.contains("multi_ack_detailed");
    if multi_ack_detailed {
        enabled.push("multi_ack_detailed");
    }
    if multi_ack_detailed && caps.contains("no-done") {
        enabled.push("no-done");
    }
    for want in [
        "side-band-64k",
        "thin-pack",
        "no-progress",
        "include-tag",
        "ofs-delta",
    ] {
        if caps.contains(want) {
            enabled.push(want);
        }
    }
    if enabled.is_empty() {
        String::new()
    } else {
        format!(" {}", enabled.join(" "))
    }
}

/// Next stateless-RPC `have` batch size (mirrors `fetch-pack.c` `next_flush`).
fn next_flush(count: usize) -> usize {
    const LARGE_FLUSH: usize = 16384;
    if count < LARGE_FLUSH {
        count * 2
    } else {
        count * 11 / 10
    }
}

/// Append the v0/v1 shallow/deepen request lines (the client's `shallow <oid>`
/// grafts and any `deepen`/`deepen-since`/`deepen-not`) to the persistent request
/// `state`, gated on the server capability where one exists. Mirrors the CLI's
/// `append_fetch_request_extensions_v0_v1`.
fn append_shallow_request_v0_http(
    req: &mut Vec<u8>,
    caps: &HashSet<String>,
    local_shallow: &[ObjectId],
    opts: &FetchOptions,
) -> Result<()> {
    for oid in local_shallow {
        pkt_line::write_line_to_vec(req, &format!("shallow {}", oid.to_hex()))?;
    }
    if opts.unshallow {
        pkt_line::write_line_to_vec(req, &format!("deepen {}", crate::shallow::INFINITE_DEPTH))?;
    } else if let Some(depth) = opts.depth.filter(|d| *d > 0) {
        pkt_line::write_line_to_vec(req, &format!("deepen {depth}"))?;
    }
    if let Some(since) = opts
        .deepen_since
        .as_deref()
        .filter(|s| !s.trim().is_empty())
    {
        if caps.contains("deepen-since") {
            let value = crate::shallow::deepen_since_wire_value(since);
            pkt_line::write_line_to_vec(req, &format!("deepen-since {value}"))?;
        }
    }
    if caps.contains("deepen-not") {
        for excl in &opts.deepen_not {
            let excl = excl.trim();
            if !excl.is_empty() {
                pkt_line::write_line_to_vec(req, &format!("deepen-not {excl}"))?;
            }
        }
    }
    Ok(())
}

/// Negotiate and download the pack for `wants` over stateless-RPC HTTP,
/// returning the raw pack bytes (empty if the server sent none) plus any
/// shallow-boundary updates the server reported.
#[expect(clippy::too_many_arguments)]
fn negotiate_pack_http(
    client: &dyn HttpClient,
    local_git_dir: &Path,
    repo_url: &str,
    caps: &HashSet<String>,
    advertised: &[AdvRef],
    wants: &[ObjectId],
    opts: &FetchOptions,
    local_shallow: &[ObjectId],
    progress: &mut dyn Progress,
) -> Result<(Option<std::path::PathBuf>, crate::fetch::ShallowUpdate)> {
    let post_url = upload_pack_url(repo_url);
    let content_type = format!("application/x-{UPLOAD_PACK}-request");
    let accept = format!("application/x-{UPLOAD_PACK}-result");
    let fetch_caps = build_fetch_caps(caps);
    let sideband = caps.contains("side-band-64k");
    let multi_ack_detailed = caps.contains("multi_ack_detailed");
    let no_done = multi_ack_detailed && caps.contains("no-done");

    // A deepen/shallow request precedes the pack with a `shallow-info` section and
    // does not offer local haves (its objects bottom out at grafts).
    let shallow_request = opts.has_deepen_request() || !local_shallow.is_empty();

    let want_set: HashSet<ObjectId> = wants.iter().copied().collect();

    // Build the persistent request prefix replayed on every RPC: the want lines
    // (capabilities on the first), the shallow/deepen extensions, and the
    // terminating flush.
    let mut state = Vec::new();
    let first = wants[0];
    pkt_line::write_line_to_vec(
        &mut state,
        &format!("want {}{}", first.to_hex(), fetch_caps),
    )?;
    for w in wants.iter().skip(1) {
        pkt_line::write_line_to_vec(&mut state, &format!("want {}", w.to_hex()))?;
    }
    append_shallow_request_v0_http(&mut state, caps, local_shallow, opts)?;
    pkt_line::write_flush(&mut state)?;

    let mut shallow_update = crate::fetch::ShallowUpdate::default();

    // Build the negotiator from local tips, marking advertised tips we already
    // have as known-common. Skipped for a shallow request.
    let local_repo = crate::repo::Repository::open(local_git_dir, None)?;
    let mut negotiator = SkippingNegotiator::new(local_repo);
    if !shallow_request {
        for w in wants {
            if negotiator.repo().odb.read(w).is_ok() {
                negotiator.add_tip(*w)?;
            }
        }
        let mut tips: Vec<ObjectId> = Vec::new();
        for prefix in ["refs/heads/", "refs/tags/"] {
            if let Ok(entries) = crate::refs::list_refs(local_git_dir, prefix) {
                for (_, oid) in entries {
                    if negotiator.repo().odb.read(&oid).is_ok() {
                        tips.push(oid);
                    }
                }
            }
        }
        if let Ok(h) = crate::refs::resolve_ref(local_git_dir, "HEAD") {
            if negotiator.repo().odb.read(&h).is_ok() {
                tips.push(h);
            }
        }
        tips.sort_by_key(ObjectId::to_hex);
        tips.dedup();
        for t in tips {
            if want_set.contains(&t) {
                continue;
            }
            negotiator.add_tip(t)?;
        }
        for e in advertised {
            if want_set.contains(&e.oid) {
                continue;
            }
            if negotiator.repo().odb.read(&e.oid).is_ok() {
                negotiator.known_common(e.oid)?;
            }
        }
    }

    let local_odb = open_odb(local_git_dir);
    let mut pack_receive = crate::pack_receive::TempPackReceive::create(local_odb.objects_dir())?;
    let mut got_ready = false;
    let mut got_pack = false;
    let mut shallow_applied = false;

    const INITIAL_FLUSH: usize = 16;
    let mut count: usize = 0;
    let mut flush_at: usize = INITIAL_FLUSH;
    let mut round = Vec::new();
    // The negotiator is empty for a shallow request, so this loop is skipped and
    // the single `done` RPC below carries the wants + shallow lines.
    while let Some(oid) = negotiator.next_have()? {
        pkt_line::write_line_to_vec(&mut round, &format!("have {}", oid.to_hex()))?;
        count += 1;
        if count < flush_at {
            continue;
        }
        flush_at = next_flush(count);

        let mut req = state.clone();
        req.extend_from_slice(&round);
        pkt_line::write_flush(&mut req)?;
        round.clear();

        let mut reader = client.post_into_reader(&post_url, &content_type, &accept, &req, None)?;
        let round_result = read_stateless_response_stream(
            &mut reader,
            sideband,
            shallow_request,
            &mut pack_receive,
            progress,
        )?;
        if shallow_request && !shallow_applied {
            shallow_update
                .shallow
                .extend(round_result.shallow.iter().copied());
            shallow_update
                .unshallow
                .extend(round_result.unshallow.iter().copied());
            shallow_applied = true;
        }
        for ack in &round_result.acks {
            if matches!(ack.kind, AckKind::Bare) {
                continue;
            }
            let was_common = negotiator.ack(ack.oid)?;
            if matches!(ack.kind, AckKind::Common) && !was_common {
                pkt_line::write_line_to_vec(&mut state, &format!("have {}", ack.oid.to_hex()))?;
            }
            if matches!(ack.kind, AckKind::Ready) {
                got_ready = true;
            }
        }
        if round_result.got_pack {
            got_pack = true;
            break;
        }
        if got_ready {
            break;
        }
    }

    // Final RPC ending in `done`, unless the pack already arrived with
    // `ACK ... ready` under `no-done`.
    if !(got_pack || got_ready && no_done) {
        let mut req = state.clone();
        pkt_line::write_line_to_vec(&mut req, "done")?;
        pkt_line::write_flush(&mut req)?;
        let mut reader = client.post_into_reader(&post_url, &content_type, &accept, &req, None)?;
        let round_result = read_stateless_response_stream(
            &mut reader,
            sideband,
            shallow_request,
            &mut pack_receive,
            progress,
        )?;
        if shallow_request && !shallow_applied {
            shallow_update.shallow.extend(round_result.shallow);
            shallow_update.unshallow.extend(round_result.unshallow);
        }
        if round_result.got_pack {
            got_pack = true;
        }
    }

    let pack_path = if got_pack {
        pack_receive.finish()?
    } else {
        let _ = pack_receive.finish()?;
        None
    };
    Ok((pack_path, shallow_update))
}

/// Resolve the `wants` for a fetch from the advertised refs and the matched set.
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
    opts: &FetchOptions,
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

    let mut shallow_update = crate::fetch::ShallowUpdate::default();
    let mut pack_oids = std::collections::HashSet::new();

    if !need.is_empty() && !opts.dry_run {
        let deepen = crate::fetch::V2DeepenArgs::from_opts(opts, &local_shallow);
        let (pack, su) = negotiate_pack_v2_http(
            client,
            local_git_dir,
            &post_url,
            &content_type,
            &accept,
            git_protocol,
            &server_caps,
            &local_odb,
            &need,
            &deepen,
            progress,
        )?;
        shallow_update = su;
        if let Some(pack_path) = pack {
            pack_oids = crate::index_pack::ingest_received_pack_path(
                pack_path,
                &local_odb,
                &crate::index_pack::IngestPackOptions {
                    fix_thin: true,
                    ..Default::default()
                },
            )?
            .object_ids;
        }
    }

    // Apply shallow/unshallow boundary updates to the on-disk `shallow` file.
    if !opts.dry_run {
        crate::shallow::apply_shallow_updates(
            local_git_dir,
            &shallow_update.shallow,
            &shallow_update.unshallow,
        )?;
    }

    // 6. For TagMode::Following, drop tags whose target did not arrive.
    if opts.tags == TagMode::Following {
        crate::fetch::retain_following_tags(
            &local_odb,
            &mut matched,
            &pack_oids,
            &crate::shallow::load_shallow_boundaries(local_git_dir),
        )?;
    }

    // 7. Classify + apply ref updates (shared with the v0/v1 path).
    let local_repo = if opts.dry_run {
        None
    } else {
        crate::repo::Repository::open(local_git_dir, None).ok()
    };

    let mut updates: Vec<RefUpdate> = Vec::new();
    if opts.prune {
        prune_tracking_refs(
            local_git_dir,
            &positive,
            &remote_refs,
            opts.dry_run,
            &mut updates,
        )?;
    }

    for m in &matched {
        let Some(local_ref) = &m.local_ref else {
            updates.push(RefUpdate {
                remote_ref: m.remote_ref.clone(),
                local_ref: None,
                old_oid: None,
                new_oid: Some(m.oid),
                mode: UpdateMode::NoChangeNeeded,
                note: Some("not stored (empty destination)".to_owned()),
            });
            continue;
        };
        let old = crate::refs::resolve_ref(local_git_dir, local_ref).ok();
        let mode = classify_update(old.as_ref(), &m.oid, m.force, m.is_tag, local_repo.as_ref());
        let write = matches!(
            mode,
            UpdateMode::New | UpdateMode::FastForward | UpdateMode::Forced
        );
        if write && !opts.dry_run {
            crate::refs::write_ref(local_git_dir, local_ref, &m.oid)?;
        }
        updates.push(RefUpdate {
            remote_ref: m.remote_ref.clone(),
            local_ref: Some(local_ref.clone()),
            old_oid: old,
            new_oid: Some(m.oid),
            mode,
            note: None,
        });
    }

    crate::net_trace::net_trace!(
        opts.network_trace,
        opts.diagnostics.as_ref(),
        "http_fetch (v2): done — {} ref update(s)",
        updates.len()
    );
    crate::fetch::finish_initial_remote_fetch_layout(
        local_git_dir,
        opts,
        default_branch.as_deref(),
    )?;
    Ok(FetchOutcome {
        updates,
        default_branch,
        new_shallow: shallow_update.shallow,
        new_unshallow: shallow_update.unshallow,
    })
}

/// Negotiate and download the pack for `wants` over stateless-RPC HTTP using
/// protocol v2 (`command=fetch`), returning the raw pack bytes.
///
/// Stateless: every POST resends the capability echo, every `want`, and all the
/// `have`s accumulated so far. The round structure mirrors the v0/v1 stateless
/// loop and the streaming v2 path:
///
/// * no local history → a single POST with `want`s + `done`, then read the
///   `packfile` section;
/// * otherwise → batched rounds that send `want`s + the growing have-prefix
///   *without* `done`, reading the `acknowledgments` section each time. When the
///   server replies `ready`, that same response carries the pack (read it and
///   stop). If the haves are exhausted without `ready`, a final POST sends every
///   have + `done` and reads the pack.
#[allow(clippy::too_many_arguments)]
fn negotiate_pack_v2_http(
    client: &dyn HttpClient,
    local_git_dir: &Path,
    post_url: &str,
    content_type: &str,
    accept: &str,
    git_protocol: &str,
    server_caps: &[String],
    local_odb: &crate::odb::Odb,
    wants: &[ObjectId],
    deepen: &crate::fetch::V2DeepenArgs,
    progress: &mut dyn Progress,
) -> Result<(Option<std::path::PathBuf>, crate::fetch::ShallowUpdate)> {
    if wants.is_empty() {
        return Ok((None, crate::fetch::ShallowUpdate::default()));
    }
    let object_format = crate::fetch::v2_object_format(server_caps, local_odb);
    let cap_echo = protocol_v2::cap_lines_for_command_request(server_caps);
    let sideband_all = protocol_v2::fetch_supports_sideband_all(server_caps);

    // A deepen/shallow request does not offer haves (its objects bottom out at
    // grafts), forcing the single-round path so the server precedes the pack with
    // a `shallow-info` section.
    let shallow_request = deepen.is_shallow_request();

    // The ordered have list, built with the shared skipping-negotiator helper so
    // the wire offers match the streaming v2 path exactly. Empty for a shallow
    // request.
    let haves = if shallow_request {
        Vec::new()
    } else {
        crate::fetch::v2_local_haves(local_git_dir, wants)?
    };

    let mut pack_receive = crate::pack_receive::TempPackReceive::create(local_odb.objects_dir())?;
    let mut shallow_update = crate::fetch::ShallowUpdate::default();
    let mut scratch = Vec::new();
    let mut got_pack = false;

    let mut read_pack_from_reader = |reader: &mut dyn Read| -> Result<()> {
        scratch.clear();
        crate::fetch::read_v2_fetch_pack_response(
            reader,
            &mut scratch,
            Some(&mut pack_receive),
            &mut shallow_update,
            progress,
            sideband_all,
        )?;
        got_pack = true;
        Ok(())
    };

    // No local history: one POST, wants + done, then the pack.
    if haves.is_empty() {
        let mut req = Vec::new();
        crate::fetch::write_v2_fetch_request(
            &mut req,
            &object_format,
            &cap_echo,
            wants,
            &[],
            sideband_all,
            deepen,
            true,
        )?;
        let mut reader =
            client.post_into_reader(post_url, content_type, accept, &req, Some(git_protocol))?;
        read_pack_from_reader(&mut reader)?;
        let pack_path = if got_pack {
            pack_receive.finish()?
        } else {
            let _ = pack_receive.finish()?;
            None
        };
        return Ok((pack_path, shallow_update));
    }

    // Batched negotiation: each round resends wants + the accumulated have prefix
    // (stateless) without `done`, reading the acknowledgments section. The flush
    // schedule matches `fetch-pack.c` (`next_flush`).
    const INITIAL_FLUSH: usize = 16;
    let mut flush_at: usize = INITIAL_FLUSH.min(haves.len());
    loop {
        if flush_at < haves.len() {
            // Non-final round: offer the have prefix [0..flush_at) without `done`.
            let mut req = Vec::new();
            crate::fetch::write_v2_fetch_request(
                &mut req,
                &object_format,
                &cap_echo,
                wants,
                &haves[..flush_at],
                sideband_all,
                deepen,
                false,
            )?;
            let resp = client.post(post_url, content_type, accept, &req, Some(git_protocol))?;
            let mut cur = Cursor::new(resp);
            let ack = crate::fetch::read_v2_acknowledgments(&mut cur, sideband_all)?;
            if let Some(round) = ack {
                if round.ready {
                    read_pack_from_reader(&mut cur)?;
                    let pack_path = if got_pack {
                        pack_receive.finish()?
                    } else {
                        let _ = pack_receive.finish()?;
                        None
                    };
                    return Ok((pack_path, shallow_update));
                }
            } else {
                read_pack_from_reader(&mut cur)?;
                let pack_path = if got_pack {
                    pack_receive.finish()?
                } else {
                    let _ = pack_receive.finish()?;
                    None
                };
                return Ok((pack_path, shallow_update));
            }
            flush_at = next_flush(flush_at).min(haves.len());
            continue;
        }

        // Final round: send every have + `done`, then read the pack.
        let mut req = Vec::new();
        crate::fetch::write_v2_fetch_request(
            &mut req,
            &object_format,
            &cap_echo,
            wants,
            &haves,
            sideband_all,
            deepen,
            true,
        )?;
        let mut reader =
            client.post_into_reader(post_url, content_type, accept, &req, Some(git_protocol))?;
        read_pack_from_reader(&mut reader)?;
        let pack_path = if got_pack {
            pack_receive.finish()?
        } else {
            let _ = pack_receive.finish()?;
            None
        };
        return Ok((pack_path, shallow_update));
    }
}

/// Convenience: the unused-by-default [`Advertisement`] shape, exported so an
/// embedder can reuse the same structured view as the duplex transports.
pub fn discovery_advertisement(conn: &SmartHttpConnection) -> Advertisement {
    Advertisement {
        refs: conn.adv_refs.clone(),
        capabilities: conn.caps.clone(),
        head_symref: conn.head_symref.clone(),
        protocol_version: conn.protocol_version,
    }
}

#[cfg(test)]
#[path = "http_discovery_tests.rs"]
mod discovery_tests;
