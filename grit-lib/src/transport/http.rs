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
//! A default [`ureq`]-backed [`HttpClient`] lives in [`crate::transport::http::ureq_client`]
//! behind the `http-ureq` cargo feature; it wires a [`CredentialProvider`] for
//! HTTP basic auth on `401`.
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
use std::io::{Cursor, Read, Write};
use std::path::Path;

use crate::error::{Error, Result};
use crate::fetch::Progress;
use crate::fetch_negotiator::SkippingNegotiator;
use crate::objects::ObjectId;
use crate::pkt_line;
use crate::protocol_v2;
use crate::refspec::{parse_fetch_refspec, RefspecItem};
use crate::transfer::{
    classify_update, match_positive, open_odb, prune_tracking_refs, ref_excluded, refspecs_force,
    FetchOptions, FetchOutcome, RefUpdate, TagMode, UpdateMode,
};
use crate::transport::{Advertisement, Connection, ConnectOptions, Service, Transport};

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
}

/// Forward [`HttpClient`] through a shared [`std::sync::Arc`], so one client can
/// back several transports (and be observed by the caller) without moving it.
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
                                head_symref = Some(target.to_owned());
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
                refs.push(AdvRef {
                    name: refname.to_owned(),
                    oid,
                });
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
    let base = repo_url.trim_end_matches('/');
    let mut url = format!("{base}/info/refs");
    url.push_str(if url.contains('?') { "&" } else { "?" });
    url.push_str("service=");
    url.push_str(UPLOAD_PACK);
    url
}

/// The `git-upload-pack` stateless-RPC endpoint URL for `repo_url`.
fn upload_pack_url(repo_url: &str) -> String {
    let base = repo_url.trim_end_matches('/');
    format!("{base}/{UPLOAD_PACK}")
}

/// A live smart-HTTP connection: the parsed advertisement plus the context
/// needed to issue the stateless-RPC POST. Smart HTTP is request/response, so
/// there is no persistent duplex socket — the `reader`/`writer` accessors are
/// not used by [`http_fetch`], which drives the POST loop directly.
///
/// `reader`/`writer` return empty/sink streams; embedders that want to drive a
/// custom negotiation should use [`http_fetch`] (or read the advertisement via
/// the accessors and POST through their [`HttpClient`]).
pub struct SmartHttpConnection {
    repo_url: String,
    adv_refs: Vec<(String, ObjectId)>,
    caps: Vec<String>,
    head_symref: Option<String>,
    protocol_version: u8,
    object_format: String,
    // Held so embedders/tests can identify which service this connection speaks.
    service: Service,
    empty_reader: Cursor<Vec<u8>>,
    sink: Vec<u8>,
}

impl SmartHttpConnection {
    /// The repository URL this connection targets.
    #[must_use]
    pub fn repo_url(&self) -> &str {
        &self.repo_url
    }

    /// The server's advertised object format (`sha1` or `sha256`).
    #[must_use]
    pub fn object_format(&self) -> &str {
        &self.object_format
    }

    /// The service this connection speaks.
    #[must_use]
    pub fn service(&self) -> Service {
        self.service
    }
}

impl Connection for SmartHttpConnection {
    fn reader(&mut self) -> &mut dyn Read {
        &mut self.empty_reader
    }

    fn writer(&mut self) -> &mut dyn Write {
        &mut self.sink
    }

    fn advertised_refs(&self) -> &[(String, ObjectId)] {
        &self.adv_refs
    }

    fn capabilities(&self) -> &[String] {
        &self.caps
    }

    fn head_symref(&self) -> Option<&str> {
        self.head_symref.as_deref()
    }

    fn protocol_version(&self) -> u8 {
        self.protocol_version
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

impl<C: HttpClient> SmartHttpTransport<C> {
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
        crate::push::push_http(&self.client, local_git_dir, repo_url, refs, opts, progress)
    }

    /// Perform the `info/refs` discovery for `repo_url` and `service`, returning
    /// the parsed [`Discovery`].
    ///
    /// `git_protocol` is the `Git-Protocol` request-header value to apply (e.g.
    /// `version=2` to request a v2 advertisement); when `None`, the client's
    /// default ([`HttpClient::git_protocol_header`]) is used.
    fn discover(
        &self,
        repo_url: &str,
        _service: Service,
        git_protocol: Option<&str>,
    ) -> Result<Discovery> {
        let url = info_refs_url(repo_url);
        let gp = git_protocol.or_else(|| self.client.git_protocol_header());
        let body = self.client.get(&url, gp)?;
        let stripped = strip_service_advertisement(&body)?;
        parse_advertisement(stripped)
    }
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

impl<C: HttpClient> Transport for SmartHttpTransport<C> {
    fn connect(
        &self,
        url: &str,
        service: Service,
        opts: &ConnectOptions,
    ) -> Result<Box<dyn Connection>> {
        // Request the protocol version the caller asked for via the
        // `Git-Protocol` header (a v2 server only returns its v2 capability
        // advertisement when it sees `version=2`); fall back to the client's
        // default header otherwise. The server may still downgrade.
        crate::net_trace::net_trace!(
            "http(s) discover {url} (service={}, request protocol v{})",
            service.wire_name(),
            opts.protocol_version
        );
        let gp = git_protocol_for_version(opts.protocol_version);
        let disc = self.discover(url, service, gp.as_deref())?;
        let adv_refs: Vec<(String, ObjectId)> = disc
            .refs
            .iter()
            .filter(|r| r.name != "HEAD" && !r.name.ends_with("^{}"))
            .map(|r| (r.name.clone(), r.oid))
            .collect();
        let caps: Vec<String> = disc.caps.iter().cloned().collect();
        crate::net_trace::net_trace!(
            "http(s) discovered: protocol v{}, {} ref(s) advertised",
            disc.protocol_version,
            adv_refs.len()
        );
        Ok(Box::new(SmartHttpConnection {
            repo_url: url.to_owned(),
            adv_refs,
            caps,
            head_symref: disc.head_symref,
            protocol_version: disc.protocol_version,
            object_format: disc.object_format,
            service,
            empty_reader: Cursor::new(Vec::new()),
            sink: Vec::new(),
        }))
    }
}

/// Read a length-prefixed pkt-line payload, returning `None` on flush/delim/EOF.
fn read_pkt_payload(r: &mut impl Read) -> std::io::Result<Option<Vec<u8>>> {
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

/// Demultiplex the side-band pack from a stateless-RPC response, appending pack
/// bytes to `out` and forwarding channel-2 progress. Mirrors the CLI's
/// `read_sideband_pack_until_done`.
fn read_sideband_pack(
    r: &mut impl Read,
    out: &mut Vec<u8>,
    progress: &mut dyn Progress,
) -> Result<()> {
    let mut seen_pack = false;
    let mut pending: Vec<u8> = Vec::new();
    loop {
        let mut len_buf = [0u8; 4];
        match r.read_exact(&mut len_buf) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(e.into()),
        }
        let len_str = std::str::from_utf8(&len_buf)
            .map_err(|_| Error::Message("bad pkt length".to_owned()))?;
        let len = usize::from_str_radix(len_str, 16)
            .map_err(|_| Error::Message("bad pkt length".to_owned()))?;
        match len {
            0 => {
                if seen_pack {
                    break;
                }
                continue;
            }
            1 | 2 => continue,
            n if n <= 4 => {
                return Err(Error::Message(format!(
                    "invalid pkt-line length in side-band stream: {n}"
                )))
            }
            _ => {}
        }
        let mut payload = vec![0u8; len - 4];
        r.read_exact(&mut payload)?;
        if payload.is_empty() {
            continue;
        }
        match payload[0] {
            1 => append_pack_data(&payload[1..], out, &mut pending, &mut seen_pack),
            2 => progress.message(&payload[1..]),
            3 => {
                return Err(Error::Message(format!(
                    "remote error: {}",
                    String::from_utf8_lossy(&payload[1..]).trim_end()
                )))
            }
            _ => append_pack_data(&payload, out, &mut pending, &mut seen_pack),
        }
    }
    Ok(())
}

/// Append channel-1 (or raw) data to `out`, scanning for the `PACK` magic that
/// may straddle chunk boundaries.
fn append_pack_data(data: &[u8], out: &mut Vec<u8>, pending: &mut Vec<u8>, seen_pack: &mut bool) {
    if *seen_pack {
        out.extend_from_slice(data);
        return;
    }
    pending.extend_from_slice(data);
    if let Some(pos) = pending.windows(4).position(|w| w == b"PACK") {
        *seen_pack = true;
        out.extend_from_slice(&pending[pos..]);
        pending.clear();
    } else if pending.len() > 3 {
        let keep_from = pending.len() - 3;
        pending.drain(..keep_from);
    }
}

/// Parse one v0 stateless-RPC `git-upload-pack` response: an optional leading
/// `shallow-info` section (only when `expect_shallow`, i.e. a deepen was
/// requested), then optional `ACK`/`NAK` negotiation lines, then (if the server
/// is generating one) the side-band pack.
fn read_stateless_response(
    resp: &[u8],
    sideband: bool,
    expect_shallow: bool,
    pack_buf: &mut Vec<u8>,
    progress: &mut dyn Progress,
) -> Result<RoundResult> {
    let mut cur = Cursor::new(resp);
    let mut acks = Vec::new();
    let mut got_pack = false;
    let mut shallow = Vec::new();
    let mut unshallow = Vec::new();

    // Shallow-info section: `shallow`/`unshallow` lines terminated by a flush. A
    // server with nothing to report still emits the trailing flush. Rewind and
    // fall through if the first line is not a shallow-info line (no section).
    if expect_shallow {
        loop {
            let start = cur.position() as usize;
            match pkt_line::read_packet(&mut cur)? {
                None | Some(pkt_line::Packet::Flush) => break,
                Some(pkt_line::Packet::Data(line)) => {
                    let line = line.trim_end_matches('\n');
                    if let Some(rest) = line.strip_prefix("shallow ") {
                        if let Ok(oid) = ObjectId::from_hex(rest.trim()) {
                            shallow.push(oid);
                        }
                    } else if let Some(rest) = line.strip_prefix("unshallow ") {
                        if let Ok(oid) = ObjectId::from_hex(rest.trim()) {
                            unshallow.push(oid);
                        }
                    } else {
                        cur.set_position(start as u64);
                        break;
                    }
                }
                Some(_) => break,
            }
        }
    }

    loop {
        let start = cur.position() as usize;
        let Some(payload) = read_pkt_payload(&mut cur)? else {
            break;
        };
        if payload.is_empty() {
            continue;
        }
        let is_pack = (sideband
            && payload.first() == Some(&1)
            && payload.get(1..5) == Some(b"PACK"))
            || payload.starts_with(b"PACK");
        if is_pack {
            got_pack = true;
            cur.set_position(start as u64);
            if sideband {
                read_sideband_pack(&mut cur, pack_buf, progress)?;
            } else {
                pack_buf.extend_from_slice(&resp[start..]);
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
    if let Some(since) = opts.deepen_since.as_deref().filter(|s| !s.trim().is_empty()) {
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
) -> Result<(Vec<u8>, crate::fetch::ShallowUpdate)> {
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
    pkt_line::write_line_to_vec(&mut state, &format!("want {}{}", first.to_hex(), fetch_caps))?;
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

    let mut pack_buf: Vec<u8> = Vec::new();
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

        let resp = client.post(&post_url, &content_type, &accept, &req, None)?;
        let round_result =
            read_stateless_response(&resp, sideband, shallow_request, &mut pack_buf, progress)?;
        if shallow_request && !shallow_applied {
            shallow_update.shallow.extend(round_result.shallow.iter().copied());
            shallow_update.unshallow.extend(round_result.unshallow.iter().copied());
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
        let resp = client.post(&post_url, &content_type, &accept, &req, None)?;
        let round_result =
            read_stateless_response(&resp, sideband, shallow_request, &mut pack_buf, progress)?;
        if shallow_request && !shallow_applied {
            shallow_update.shallow.extend(round_result.shallow);
            shallow_update.unshallow.extend(round_result.unshallow);
        }
    }

    Ok((pack_buf, shallow_update))
}

/// Resolve the `wants` for a fetch from the advertised refs and the matched set.
///
/// Returns the matched ref records (for later ref-update classification) and the
/// set of wanted oids.
struct MatchPlan {
    matched: Vec<crate::transfer::MatchedRef>,
    wants: HashSet<ObjectId>,
    seen: HashSet<String>,
}

fn match_refspecs(
    remote_refs: &[(String, ObjectId)],
    positive: &[RefspecItem],
    negatives: &[RefspecItem],
) -> MatchPlan {
    let mut matched: Vec<crate::transfer::MatchedRef> = Vec::new();
    let mut wants: HashSet<ObjectId> = HashSet::new();
    let mut seen: HashSet<String> = HashSet::new();
    for (name, oid) in remote_refs {
        if ref_excluded(name, negatives) {
            continue;
        }
        if let Some(local_ref) = match_positive(name, positive) {
            if seen.insert(name.clone()) {
                wants.insert(*oid);
                matched.push(crate::transfer::MatchedRef {
                    remote_ref: name.clone(),
                    local_ref,
                    oid: *oid,
                    force: refspecs_force(name, positive),
                    is_tag: name.starts_with("refs/tags/"),
                });
            }
        }
    }
    MatchPlan {
        matched,
        wants,
        seen,
    }
}

/// Fetch from a smart-HTTP remote, driving the stateless-RPC negotiation and
/// writing tracking-ref updates into `local_git_dir`.
///
/// This is the HTTP counterpart to [`crate::fetch::fetch_remote`]: instead of a
/// duplex socket it issues `info/refs` discovery + `git-upload-pack` POSTs
/// through `client`. The refspec matching, tag-mode, prune, and update
/// classification reuse the shared [`crate::transfer`] helpers, so the
/// [`FetchOutcome`] shape matches every other fetch path.
///
/// Both protocol v0/v1 and protocol v2 are handled: the version is taken from
/// the `info/refs` advertisement (the v2 capability block is returned only when
/// the discovery GET carries `Git-Protocol: version=2`, which the client's
/// default header supplies). For v2 the ref map is recovered with a
/// `command=ls-refs` POST and the pack is negotiated with `command=fetch` POSTs
/// (stateless: every round resends the wants + accumulated haves).
///
/// # Errors
///
/// Returns an error if discovery fails, a refspec is invalid, or negotiation /
/// pack ingest / ref I/O fails.
pub fn http_fetch(
    client: &dyn HttpClient,
    local_git_dir: &Path,
    repo_url: &str,
    opts: &FetchOptions,
    progress: &mut dyn Progress,
) -> Result<FetchOutcome> {
    use crate::net_trace::net_trace;
    net_trace!(
        "http_fetch: begin — {} ({} refspec(s), tags={:?})",
        repo_url,
        opts.refspecs.len(),
        opts.tags
    );
    // 1. Discovery (request v2 via the client's default `Git-Protocol` header;
    // a v0/v1 server ignores it and returns the classic advertisement).
    let disc = {
        let url = info_refs_url(repo_url);
        let body = client.get(&url, client.git_protocol_header())?;
        let stripped = strip_service_advertisement(&body)?;
        parse_advertisement(stripped)?
    };
    net_trace!(
        "http_fetch: discovered protocol v{}, {} ref(s)",
        disc.protocol_version,
        disc.refs.len()
    );
    if disc.protocol_version >= 2 {
        net_trace!("http_fetch: delegating to v2 stateless fetch");
        return http_fetch_v2(client, local_git_dir, repo_url, &disc, opts, progress);
    }

    let local_odb = open_odb(local_git_dir);

    let default_branch = disc
        .head_symref
        .as_deref()
        .map(|t| t.strip_prefix("refs/heads/").unwrap_or(t).to_owned());

    let remote_refs: Vec<(String, ObjectId)> = disc
        .refs
        .iter()
        .filter(|r| r.name != "HEAD" && !r.name.ends_with("^{}"))
        .map(|r| (r.name.clone(), r.oid))
        .collect();

    // 2. Parse refspecs.
    let mut positive: Vec<RefspecItem> = Vec::new();
    let mut negatives: Vec<RefspecItem> = Vec::new();
    for spec in &opts.refspecs {
        let item = parse_fetch_refspec(spec)
            .map_err(|e| Error::Message(format!("invalid refspec '{spec}': {e}")))?;
        if item.negative {
            negatives.push(item);
        } else {
            positive.push(item);
        }
    }
    for spec in &opts.negative_refspecs {
        let item = parse_fetch_refspec(spec)
            .map_err(|e| Error::Message(format!("invalid negative refspec '{spec}': {e}")))?;
        negatives.push(item);
    }

    // 3. Match refs to refspecs.
    let MatchPlan {
        mut matched,
        mut wants,
        mut seen,
    } = match_refspecs(&remote_refs, &positive, &negatives);

    // 4. TagMode: add tags (the wire `include-tag` capability brings tag
    // objects with the pack; All adds every advertised tag, Following adds them
    // provisionally and prunes unreachable ones after the pack lands).
    if opts.tags != TagMode::None {
        for (name, oid) in &remote_refs {
            if !name.starts_with("refs/tags/") {
                continue;
            }
            if seen.contains(name) || ref_excluded(name, &negatives) {
                continue;
            }
            seen.insert(name.clone());
            wants.insert(*oid);
            matched.push(crate::transfer::MatchedRef {
                remote_ref: name.clone(),
                local_ref: Some(name.clone()),
                oid: *oid,
                force: false,
                is_tag: true,
            });
        }
    }

    // 5. Wants → negotiate + ingest the pack. Normally the matched oids absent
    // locally; for a deepen/`--unshallow` request we must still `want` the tips
    // even if present so the server fills in ancestors past the old boundary.
    let local_shallow = crate::shallow::load_shallow_oids(local_git_dir)?;
    let shallow_request = opts.has_deepen_request() || !local_shallow.is_empty();
    let need: Vec<ObjectId> = if shallow_request {
        wants.iter().copied().collect()
    } else {
        wants
            .iter()
            .copied()
            .filter(|oid| !local_odb.exists(oid))
            .collect()
    };

    let mut shallow_update = crate::fetch::ShallowUpdate::default();

    if !need.is_empty() && !opts.dry_run {
        let (pack, su) = negotiate_pack_http(
            client,
            local_git_dir,
            repo_url,
            &disc.caps,
            &disc.refs,
            &need,
            opts,
            &local_shallow,
            progress,
        )?;
        shallow_update = su;
        if !pack.is_empty() {
            if pack.len() < 12 || &pack[0..4] != b"PACK" {
                return Err(Error::Message(
                    "did not receive a valid pack from HTTP fetch".to_owned(),
                ));
            }
            let mut cursor = Cursor::new(pack);
            crate::unpack_objects::unpack_objects(
                &mut cursor,
                &local_odb,
                &crate::unpack_objects::UnpackOptions {
                    quiet: true,
                    ..Default::default()
                },
            )?;
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
        retain_following_tags(&local_odb, &mut matched, &wants);
    }

    // 7. Classify + apply ref updates.
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

    net_trace!("http_fetch: done — {} ref update(s)", updates.len());
    Ok(FetchOutcome {
        updates,
        default_branch,
        new_shallow: shallow_update.shallow,
        new_unshallow: shallow_update.unshallow,
    })
}

/// Fetch from a smart-HTTP remote that speaks protocol v2 (stateless multi-POST).
///
/// `disc` is the already-parsed v2 capability advertisement (no refs). This
/// recovers the ref map with a `command=ls-refs` POST, matches refspecs / tags
/// with the same shared [`crate::transfer`] helpers as the v0/v1 path, then
/// negotiates the pack with `command=fetch` POSTs (each round resends the
/// capability echo, all `want`s, and the accumulated `have`s) and demuxes the
/// side-band-64k `packfile` section. Lifted from the CLI's stateless v2 flow
/// (`http_ls_refs` / `http_negotiate_only_common` / `http_fetch_pack`), reusing
/// the v2 request framing factored out of [`crate::fetch`].
fn http_fetch_v2(
    client: &dyn HttpClient,
    local_git_dir: &Path,
    repo_url: &str,
    disc: &Discovery,
    opts: &FetchOptions,
    progress: &mut dyn Progress,
) -> Result<FetchOutcome> {
    let local_odb = open_odb(local_git_dir);
    // The v2 capability lines, as a `Vec<String>` for the `protocol_v2` /
    // `crate::fetch` helpers (each entry is one advertised capability line, e.g.
    // `agent=…`, `fetch=…`, `object-format=…`).
    let server_caps: Vec<String> = disc.caps.iter().cloned().collect();

    let post_url = upload_pack_url(repo_url);
    let content_type = format!("application/x-{UPLOAD_PACK}-request");
    let accept = format!("application/x-{UPLOAD_PACK}-result");
    // Pin v2 on every POST so the server runs its v2 serve loop for this request.
    let git_protocol = "version=2";

    // 1. Recover the ref map via `command=ls-refs`.
    let (remote_refs, head_symref) = {
        let req =
            crate::fetch::build_v2_ls_refs_request(&server_caps, &local_odb, opts.tags, &opts.refspecs)?;
        let resp = client.post(&post_url, &content_type, &accept, &req, Some(git_protocol))?;
        let mut cur = Cursor::new(resp);
        crate::fetch::parse_v2_ls_refs_response(&mut cur)?
    };
    let default_branch = head_symref
        .as_deref()
        .map(|t| t.strip_prefix("refs/heads/").unwrap_or(t).to_owned());

    // 2. Parse refspecs.
    let mut positive: Vec<RefspecItem> = Vec::new();
    let mut negatives: Vec<RefspecItem> = Vec::new();
    for spec in &opts.refspecs {
        let item = parse_fetch_refspec(spec)
            .map_err(|e| Error::Message(format!("invalid refspec '{spec}': {e}")))?;
        if item.negative {
            negatives.push(item);
        } else {
            positive.push(item);
        }
    }
    for spec in &opts.negative_refspecs {
        let item = parse_fetch_refspec(spec)
            .map_err(|e| Error::Message(format!("invalid negative refspec '{spec}': {e}")))?;
        negatives.push(item);
    }

    // 3. Match refs to refspecs (shared with the v0/v1 path).
    let MatchPlan {
        mut matched,
        mut wants,
        mut seen,
    } = match_refspecs(&remote_refs, &positive, &negatives);

    // 4. TagMode: add tags (the wire `include-tag` capability brings tag objects
    // with the pack; All adds every advertised tag, Following adds them
    // provisionally and prunes unreachable ones after the pack lands).
    if opts.tags != TagMode::None {
        for (name, oid) in &remote_refs {
            if !name.starts_with("refs/tags/") {
                continue;
            }
            if seen.contains(name) || ref_excluded(name, &negatives) {
                continue;
            }
            seen.insert(name.clone());
            wants.insert(*oid);
            matched.push(crate::transfer::MatchedRef {
                remote_ref: name.clone(),
                local_ref: Some(name.clone()),
                oid: *oid,
                force: false,
                is_tag: true,
            });
        }
    }

    // 5. Wants → negotiate + ingest the pack. Normally the matched oids absent
    // locally; for a deepen/`--unshallow` request we must still `want` the tips
    // even if present so the server fills in ancestors past the old boundary.
    let local_shallow = crate::shallow::load_shallow_oids(local_git_dir)?;
    let shallow_request = opts.has_deepen_request() || !local_shallow.is_empty();
    let need: Vec<ObjectId> = if shallow_request {
        wants.iter().copied().collect()
    } else {
        wants
            .iter()
            .copied()
            .filter(|oid| !local_odb.exists(oid))
            .collect()
    };

    let mut shallow_update = crate::fetch::ShallowUpdate::default();

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
        if !pack.is_empty() {
            if pack.len() < 12 || &pack[0..4] != b"PACK" {
                return Err(Error::Message(
                    "did not receive a valid pack from v2 HTTP fetch".to_owned(),
                ));
            }
            let mut cursor = Cursor::new(pack);
            crate::unpack_objects::unpack_objects(
                &mut cursor,
                &local_odb,
                &crate::unpack_objects::UnpackOptions {
                    quiet: true,
                    ..Default::default()
                },
            )?;
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
        retain_following_tags(&local_odb, &mut matched, &wants);
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

    crate::net_trace::net_trace!("http_fetch (v2): done — {} ref update(s)", updates.len());
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
) -> Result<(Vec<u8>, crate::fetch::ShallowUpdate)> {
    if wants.is_empty() {
        return Ok((Vec::new(), crate::fetch::ShallowUpdate::default()));
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

    let mut pack = Vec::new();
    let mut shallow_update = crate::fetch::ShallowUpdate::default();

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
        let resp = client.post(post_url, content_type, accept, &req, Some(git_protocol))?;
        let mut cur = Cursor::new(resp);
        crate::fetch::read_v2_fetch_pack_response(&mut cur, &mut pack, &mut shallow_update, progress)?;
        return Ok((pack, shallow_update));
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
            let ack = crate::fetch::read_v2_acknowledgments(&mut cur)?;
            if let Some(round) = ack {
                if round.ready {
                    // The pack follows in this same response after the delimiter.
                    crate::fetch::read_v2_fetch_pack_response(
                        &mut cur,
                        &mut pack,
                        &mut shallow_update,
                        progress,
                    )?;
                    return Ok((pack, shallow_update));
                }
            } else {
                // Server skipped acknowledgments and went straight to the pack.
                crate::fetch::read_v2_fetch_pack_response(
                    &mut cur,
                    &mut pack,
                    &mut shallow_update,
                    progress,
                )?;
                return Ok((pack, shallow_update));
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
        let resp = client.post(post_url, content_type, accept, &req, Some(git_protocol))?;
        let mut cur = Cursor::new(resp);
        crate::fetch::read_v2_fetch_pack_response(&mut cur, &mut pack, &mut shallow_update, progress)?;
        return Ok((pack, shallow_update));
    }
}

/// Drop provisional `Following` tags whose object did not arrive in the pack.
fn retain_following_tags(
    odb: &crate::odb::Odb,
    matched: &mut Vec<crate::transfer::MatchedRef>,
    wants: &HashSet<ObjectId>,
) {
    let roots: Vec<ObjectId> = matched.iter().filter(|m| !m.is_tag).map(|m| m.oid).collect();
    let closure = reachable_closure(odb, &roots);
    matched.retain(|m| {
        if !m.is_tag {
            return true;
        }
        let peeled = peel_tag_target(odb, m.oid);
        let have = odb.exists(&m.oid);
        have && (closure.contains(&m.oid) || closure.contains(&peeled) || wants.contains(&peeled))
    });
}

fn peel_tag_target(odb: &crate::odb::Odb, oid: ObjectId) -> ObjectId {
    let mut current = oid;
    for _ in 0..16 {
        let Ok(obj) = odb.read(&current) else {
            return current;
        };
        if obj.kind != crate::objects::ObjectKind::Tag {
            return current;
        }
        match crate::objects::parse_tag(&obj.data) {
            Ok(t) => current = t.object,
            Err(_) => return current,
        }
    }
    current
}

fn reachable_closure(odb: &crate::odb::Odb, roots: &[ObjectId]) -> HashSet<ObjectId> {
    use crate::objects::{parse_commit, parse_tag, parse_tree, ObjectKind};
    let mut seen: HashSet<ObjectId> = HashSet::new();
    let mut stack: Vec<ObjectId> = roots.to_vec();
    while let Some(oid) = stack.pop() {
        if !seen.insert(oid) {
            continue;
        }
        let Ok(obj) = odb.read(&oid) else {
            continue;
        };
        match obj.kind {
            ObjectKind::Commit => {
                if let Ok(c) = parse_commit(&obj.data) {
                    stack.push(c.tree);
                    for p in c.parents {
                        stack.push(p);
                    }
                }
            }
            ObjectKind::Tree => {
                if let Ok(entries) = parse_tree(&obj.data) {
                    for e in entries {
                        stack.push(e.oid);
                    }
                }
            }
            ObjectKind::Tag => {
                if let Ok(t) = parse_tag(&obj.data) {
                    stack.push(t.object);
                }
            }
            ObjectKind::Blob => {}
        }
    }
    seen
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
mod tests {
    use super::*;

    #[test]
    fn strips_smart_service_preamble() {
        let mut body = Vec::new();
        pkt_line::write_line_to_vec(&mut body, "# service=git-upload-pack\n").unwrap();
        body.extend_from_slice(b"0000");
        let oid = "1".repeat(40);
        let line = format!("{oid} refs/heads/main\0multi_ack_detailed side-band-64k");
        pkt_line::write_line_to_vec(&mut body, &line).unwrap();
        body.extend_from_slice(b"0000");

        let stripped = strip_service_advertisement(&body).unwrap();
        let disc = parse_advertisement(stripped).unwrap();
        assert_eq!(disc.protocol_version, 0);
        assert_eq!(disc.refs.len(), 1);
        assert_eq!(disc.refs[0].name, "refs/heads/main");
        assert!(disc.caps.contains("side-band-64k"));
    }

    #[test]
    fn parses_symref_and_caps() {
        let mut body = Vec::new();
        let main = "2".repeat(40);
        let head = format!(
            "{main} HEAD\0multi_ack_detailed symref=HEAD:refs/heads/main object-format=sha1"
        );
        pkt_line::write_line_to_vec(&mut body, &head).unwrap();
        let r = format!("{main} refs/heads/main");
        pkt_line::write_line_to_vec(&mut body, &r).unwrap();
        body.extend_from_slice(b"0000");

        let disc = parse_advertisement(&body).unwrap();
        assert_eq!(disc.head_symref.as_deref(), Some("refs/heads/main"));
        assert_eq!(disc.object_format, "sha1");
        // `parse_advertisement` keeps HEAD; the connection/fetch layer filters
        // HEAD and peeled `^{}` carriers. Both lines parse here.
        assert!(disc.refs.iter().any(|r| r.name == "HEAD"));
        assert!(disc.refs.iter().any(|r| r.name == "refs/heads/main"));
    }

    #[test]
    fn detects_v2_preamble() {
        let mut body = Vec::new();
        pkt_line::write_line_to_vec(&mut body, "version 2").unwrap();
        pkt_line::write_line_to_vec(&mut body, "ls-refs").unwrap();
        pkt_line::write_line_to_vec(&mut body, "object-format=sha256").unwrap();
        body.extend_from_slice(b"0000");
        let disc = parse_advertisement(&body).unwrap();
        assert_eq!(disc.protocol_version, 2);
        assert_eq!(disc.object_format, "sha256");
    }

    #[test]
    fn url_helpers() {
        assert_eq!(
            info_refs_url("http://h/r.git"),
            "http://h/r.git/info/refs?service=git-upload-pack"
        );
        assert_eq!(
            info_refs_url("http://h/r.git/"),
            "http://h/r.git/info/refs?service=git-upload-pack"
        );
        assert_eq!(upload_pack_url("http://h/r.git/"), "http://h/r.git/git-upload-pack");
    }
}
