//! Smart HTTP client for `git clone` / `git fetch` (protocol v2 over HTTP).
//!
//! Used when the repository URL is `http://` or `https://`. Emits trace2 `child_start`
//! lines compatible with `test_remote_https_urls` in the test harness.

use std::collections::HashSet;
use std::fs::OpenOptions;
use std::io::{BufReader, Cursor, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use anyhow::{bail, Context, Result};
use grit_lib::fetch_negotiator::SkippingNegotiator;
use grit_lib::merge_base;
use grit_lib::objects::ObjectId;
use grit_lib::refs;
use grit_lib::repo::Repository;
use grit_lib::rev_parse::{resolve_revision, try_peel_to_commit_for_merge_base};

use crate::http_bundle_uri::strip_v0_service_advertisement_if_present;
use crate::http_transport::{
    read_sideband_pack_to_writer, write_sideband_pack_to_temp, HttpResponseBody, SidebandProgress,
};
use grit_lib::pkt_line;

const SERVICE: &str = "git-upload-pack";

static TRACED_HTTPS_URLS: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();

/// Clear deduplication state for `GIT_TRACE2_EVENT` `child_start` lines (new top-level command).
pub fn clear_trace2_https_url_dedup() {
    if let Some(m) = TRACED_HTTPS_URLS.get() {
        if let Ok(mut g) = m.lock() {
            g.clear();
        }
    }
}

/// Emit a single JSON trace2 line (for deduplicated bundle fetches).
pub fn trace2_child_start_git_remote_https(url: &str) {
    let Ok(path) = std::env::var("GIT_TRACE2_EVENT") else {
        return;
    };
    if path.is_empty() {
        return;
    }
    let set = TRACED_HTTPS_URLS.get_or_init(|| Mutex::new(HashSet::new()));
    let mut guard = set.lock().ok();
    if let Some(ref mut g) = guard {
        if !g.insert(url.to_string()) {
            return;
        }
    }
    let now = crate::trace2_json_now();
    let safe_url = crate::http_client::scrub_url_credentials(url);
    let esc = safe_url.replace('\\', "\\\\").replace('"', "\\\"");
    let line = format!(
        r#"{{"event":"child_start","sid":"grit-0","time":"{}","argv":["git-remote-https","{}"]}}"#,
        now, esc
    );
    let _ = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .and_then(|mut f| writeln!(f, "{line}"));
}

pub(crate) fn agent_header() -> String {
    format!("grit/{}", crate::version_string())
}

fn trace_http_dest() -> Option<String> {
    let Ok(dest) = std::env::var("GRIT_TRACE_HTTP") else {
        return None;
    };
    if dest.is_empty() || dest == "0" || dest.eq_ignore_ascii_case("false") {
        return None;
    }
    Some(if dest == "1" {
        "/dev/stderr".to_string()
    } else {
        dest
    })
}

fn trace_http_line(line: impl AsRef<str>) {
    let Some(dest) = trace_http_dest() else {
        return;
    };
    let line = line.as_ref();
    if dest == "/dev/stderr" {
        let mut err = std::io::stderr().lock();
        let _ = writeln!(err, "{line}");
        return;
    }
    if let Ok(mut out) = OpenOptions::new().create(true).append(true).open(&dest) {
        let _ = writeln!(out, "{line}");
    }
}

fn trace_http_payload(prefix: &str, payload: &[u8]) {
    if trace_http_dest().is_none() {
        return;
    }
    let mut pos = 0usize;
    let mut seen_pack = false;
    while pos + 4 <= payload.len() {
        let len_hex = &payload[pos..pos + 4];
        let Ok(len_str) = std::str::from_utf8(len_hex) else {
            trace_http_line(format!("{prefix} raw {} bytes", payload.len() - pos));
            return;
        };
        let Ok(len) = usize::from_str_radix(len_str, 16) else {
            trace_http_line(format!("{prefix} raw {} bytes", payload.len() - pos));
            return;
        };
        pos += 4;
        match len {
            0 => {
                trace_http_line(format!("{prefix} 0000 flush"));
                continue;
            }
            1 => {
                trace_http_line(format!("{prefix} 0001 delim"));
                continue;
            }
            2 => {
                trace_http_line(format!("{prefix} 0002 response-end"));
                continue;
            }
            n if n < 4 => {
                trace_http_line(format!("{prefix} invalid pkt-len {n}"));
                return;
            }
            n if pos + (n - 4) <= payload.len() => {
                let data = &payload[pos..pos + (n - 4)];
                pos += n - 4;
                trace_http_packet_data(prefix, data, &mut seen_pack);
            }
            n => {
                trace_http_line(format!(
                    "{prefix} truncated pkt-len {n} with {} bytes remaining",
                    payload.len().saturating_sub(pos)
                ));
                return;
            }
        }
    }
    if pos < payload.len() {
        trace_http_line(format!(
            "{prefix} trailing raw {} bytes",
            payload.len() - pos
        ));
    }
}

fn trace_http_packet_data(prefix: &str, data: &[u8], seen_pack: &mut bool) {
    if data.first() == Some(&1) {
        let band = &data[1..];
        if !*seen_pack && band.starts_with(b"PACK") {
            *seen_pack = true;
            trace_http_line(format!(
                "{prefix} sideband[1] PACK data {} bytes",
                band.len()
            ));
        } else if *seen_pack {
            trace_http_line(format!(
                "{prefix} sideband[1] pack data {} bytes",
                band.len()
            ));
        } else {
            trace_http_line(format!("{prefix} sideband[1] {} bytes", band.len()));
        }
        return;
    }
    if data.first() == Some(&2) || data.first() == Some(&3) {
        let channel = data[0];
        let msg = String::from_utf8_lossy(&data[1..])
            .replace('\n', "\\n")
            .replace('\r', "\\r");
        trace_http_line(format!("{prefix} sideband[{channel}] {msg}"));
        return;
    }
    if !*seen_pack && data.starts_with(b"PACK") {
        *seen_pack = true;
        trace_http_line(format!("{prefix} PACK data {} bytes", data.len()));
        return;
    }
    if *seen_pack {
        trace_http_line(format!("{prefix} pack data {} bytes", data.len()));
        return;
    }
    let text = String::from_utf8_lossy(data)
        .replace('\n', "\\n")
        .replace('\r', "\\r")
        .replace('\0', "\\0");
    trace_http_line(format!("{prefix} {text}"));
}

/// Re-base a redirected `info/refs` URL onto the location to use for follow-up
/// POSTs. Shared with grit-cli via grit-lib; see
/// [`grit_lib::transport::http::rebased_base_from_redirect`] for the rationale
/// (Git's `http.followRedirects` smart-HTTP behavior).
use grit_lib::transport::http::rebased_base_from_redirect;

/// Build the `info/refs?service=git-upload-pack` discovery URL for a repo base.
fn info_refs_url(base: &str) -> String {
    let mut url = format!("{base}/info/refs");
    url.push_str(if url.contains('?') { "&" } else { "?" });
    url.push_str(&format!("service={SERVICE}"));
    url
}

/// Perform the `info/refs` discovery GET and return both the advertisement body
/// and the (possibly redirected) repo base URL to use for follow-up POSTs.
///
/// When the server redirects `info/refs` to a new location (see
/// [`rebased_base_from_redirect`]), the discovery GET is re-issued against the
/// redirected base. This re-roots the subsequent `git-upload-pack` POSTs onto
/// the new host (ureq will not follow a POST redirect) and re-attaches the
/// `?service=` query that a redirect may drop — without it the redirected GET
/// lands on the *dumb* `info/refs` response and fails to parse as a smart
/// advertisement.
///
/// `git_protocol` is the `Git-Protocol` header value for both the discovery GET
/// and its post-redirect re-fetch — pass `client.git_protocol_header()` to honor
/// the client default, or an explicit value (e.g. `Some("version=2")`) for a
/// caller that requires a specific protocol regardless of config.
pub(crate) fn http_get_discovery_with_base(
    client: &crate::http_client::HttpClientContext,
    refs_url: &str,
    original_base: &str,
    git_protocol: Option<&str>,
) -> Result<(Vec<u8>, String)> {
    trace_http_line(format!("> GET {refs_url}"));
    if let Some(v) = git_protocol {
        trace_http_line(format!("> Git-Protocol: {v}"));
    }
    let (body, final_url) = client.get_with_final_url(refs_url, git_protocol)?;
    trace_http_line(format!("< GET {refs_url} body {} bytes", body.len()));
    trace_http_payload("<", &body);

    let Some(new_base) = rebased_base_from_redirect(original_base, final_url.as_deref()) else {
        return Ok((body, original_base.to_string()));
    };
    trace_http_line(format!("< redirected base {original_base} -> {new_base}"));
    client.reset_auth_after_redirect_rebase();

    let new_refs_url = info_refs_url(&new_base);
    trace_http_line(format!("> GET {new_refs_url}"));
    if let Some(v) = git_protocol {
        trace_http_line(format!("> Git-Protocol: {v}"));
    }
    let (body, _) = client.get_with_final_url(&new_refs_url, git_protocol)?;
    trace_http_line(format!("< GET {new_refs_url} body {} bytes", body.len()));
    trace_http_payload("<", &body);
    Ok((body, new_base))
}

fn http_post(
    client: &crate::http_client::HttpClientContext,
    url: &str,
    content_type: &str,
    accept: &str,
    body: &[u8],
) -> Result<HttpResponseBody> {
    trace_http_line(format!("> POST {url}"));
    trace_http_line(format!("> Content-Type: {content_type}"));
    trace_http_line(format!("> Accept: {accept}"));
    if let Some(v) = client.git_protocol_header() {
        trace_http_line(format!("> Git-Protocol: {v}"));
    }
    trace_http_payload(">", body);
    let resp = client.post_upload_pack_with_git_protocol(
        url,
        content_type,
        accept,
        body,
        client.git_protocol_header(),
    )?;
    trace_http_line(format!("< POST {url} body {} bytes", resp.len()));
    if let Some(prefix) = resp.trace_prefix(4096) {
        trace_http_payload("<", prefix);
    }
    Ok(resp)
}

fn http_post_discovery(
    client: &crate::http_client::HttpClientContext,
    url: &str,
    content_type: &str,
    accept: &str,
    body: &[u8],
    git_protocol_header: Option<&str>,
) -> Result<HttpResponseBody> {
    trace_http_line(format!("> POST {url}"));
    trace_http_line(format!("> Content-Type: {content_type}"));
    trace_http_line(format!("> Accept: {accept}"));
    if let Some(v) = git_protocol_header {
        trace_http_line(format!("> Git-Protocol: {v}"));
    }
    trace_http_payload(">", body);
    let resp = client.post_with_git_protocol_raw(
        url,
        content_type,
        accept,
        body,
        git_protocol_header,
        false,
    )?;
    trace_http_line(format!("< POST {url} body {} bytes", resp.len()));
    if let Some(prefix) = resp.trace_prefix(4096) {
        trace_http_payload("<", prefix);
    }
    Ok(resp)
}

fn read_v2_caps(body: &[u8]) -> Result<Vec<String>> {
    let mut cur = Cursor::new(body);
    let first = match pkt_line::read_packet(&mut cur)? {
        None => bail!("empty v2 capability block"),
        Some(pkt_line::Packet::Data(s)) => s,
        Some(other) => bail!("expected version line, got {other:?}"),
    };
    if first != "version 2" {
        bail!("expected 'version 2', got {first:?}");
    }
    let mut caps = vec![first];
    loop {
        match pkt_line::read_packet(&mut cur)? {
            None => bail!("unexpected EOF in v2 capabilities"),
            Some(pkt_line::Packet::Flush) => break,
            Some(pkt_line::Packet::Data(s)) => caps.push(s),
            Some(other) => bail!("unexpected packet in v2 caps: {other:?}"),
        }
    }
    Ok(caps)
}

fn parse_v0_v1_advertisement(
    body: &[u8],
) -> Result<(Vec<LsRefEntry>, std::collections::HashSet<String>)> {
    let mut cur = Cursor::new(body);
    let mut refs = Vec::new();
    let mut caps = std::collections::HashSet::new();
    let mut first_ref_line = true;
    loop {
        match pkt_line::read_packet(&mut cur)? {
            None => break,
            Some(pkt_line::Packet::Flush) => break,
            Some(pkt_line::Packet::Data(line)) => {
                let line = line.trim_end_matches('\n');
                if line.starts_with("version ") {
                    crate::trace_packet::trace_packet_git('<', line);
                    continue;
                }
                // A shallow server's v0/v1 ref advertisement appends `shallow <oid>` trailer lines
                // after the refs (upstream `upload-pack` `advertise_shallow_grafts`). They are not
                // refs and carry no capabilities, so skip them rather than feeding `shallow` to the
                // OID parser (t5539 fetch from a shallow clone over http, protocol v0).
                if line.starts_with("shallow ") || line.starts_with("unshallow ") {
                    continue;
                }
                let (payload, cap_part) = match line.split_once('\0') {
                    Some((p, c)) => (p.trim(), Some(c)),
                    None => (line.trim(), None),
                };
                let (oid_hex, refname) = payload
                    .split_once('\t')
                    .or_else(|| payload.split_once(' '))
                    .ok_or_else(|| anyhow::anyhow!("malformed v0/v1 advertisement: {line}"))?;
                let oid_hex = oid_hex.trim();
                let refname = refname.trim();
                // Capabilities ride on the first ref line (after the NUL). Parse them before the
                // OID so an empty SHA-256 repo's unborn-HEAD carrier line (a 64-zero null OID,
                // which our 20-byte ObjectId cannot represent) still yields `object-format=sha256`.
                if first_ref_line {
                    if let Some(raw_caps) = cap_part {
                        for cap in raw_caps.split_whitespace() {
                            caps.insert(cap.to_string());
                        }
                    }
                    first_ref_line = false;
                }
                if refname.is_empty() {
                    continue;
                }
                // An all-zero OID marks the capabilities carrier for an unborn HEAD (empty repo);
                // it is not a real ref, and a non-SHA-1-width zero OID would fail `from_hex`.
                if oid_hex.bytes().all(|b| b == b'0') {
                    continue;
                }
                let oid = ObjectId::from_hex(oid_hex)
                    .with_context(|| format!("bad oid in v0/v1 advertisement: {oid_hex}"))?;
                if refname == "HEAD" || grit_lib::refs::is_valid_fetch_advertised_ref(refname) {
                    refs.push(LsRefEntry {
                        name: refname.to_string(),
                        oid,
                    });
                }
            }
            Some(other) => bail!("unexpected packet in v0/v1 advertisement: {other:?}"),
        }
    }
    Ok((refs, caps))
}

enum HttpDiscovery {
    V2 {
        caps: Vec<String>,
        object_format: String,
    },
    V0V1 {
        advertised: Vec<LsRefEntry>,
        caps: std::collections::HashSet<String>,
    },
}

fn discover_http_protocol(pkt_body: &[u8]) -> Result<HttpDiscovery> {
    let mut cur = Cursor::new(pkt_body);
    let first = match pkt_line::read_packet(&mut cur)? {
        None => bail!("empty smart-http advertisement"),
        Some(pkt_line::Packet::Data(s)) => s,
        // A lone flush is the v0 advertisement of an empty repository served by an older Git
        // whose `upload-pack --advertise-refs` omits the `capabilities^{}` carrier line. There
        // are no refs and no advertised object-format; treat it as an empty v0 advertisement so
        // the clone still completes (object-format defaults to sha1 absent any signal).
        Some(pkt_line::Packet::Flush) => {
            return Ok(HttpDiscovery::V0V1 {
                advertised: Vec::new(),
                caps: std::collections::HashSet::new(),
            });
        }
        Some(other) => bail!("unexpected first advertisement packet: {other:?}"),
    };
    if first == "version 2" {
        let caps = read_v2_caps(pkt_body)?;
        crate::trace_packet::trace_packet_git('<', "version 2");
        for cap in &caps {
            crate::trace_packet::trace_packet_git('<', cap);
        }
        crate::trace_packet::trace_packet_git('<', "0000");
        let object_format = caps
            .iter()
            .find_map(|c| c.strip_prefix("object-format="))
            .unwrap_or("sha1")
            .to_string();
        return Ok(HttpDiscovery::V2 {
            caps,
            object_format,
        });
    }
    let (advertised, caps) = parse_v0_v1_advertisement(pkt_body)?;
    Ok(HttpDiscovery::V0V1 { advertised, caps })
}

fn trace_http_v0_v1_negotiated(client: &crate::http_client::HttpClientContext) {
    if matches!(client.git_protocol_header(), Some("version=1")) {
        crate::trace_packet::trace_packet_git('<', "version 1");
    }
}

use grit_lib::protocol_v2::cap_lines_for_command_request as cap_lines_for_client_request;

fn skip_to_flush(r: &mut impl Read) -> Result<()> {
    loop {
        match pkt_line::read_packet(r)? {
            None => return Ok(()),
            Some(pkt_line::Packet::Flush) => return Ok(()),
            Some(pkt_line::Packet::Data(_)) => {}
            Some(_) => {}
        }
    }
}

/// Read a v2 `wanted-refs` section (`<oid> <refname>` lines, delim/flush terminated) and override
/// the OID of matching entries in `heads`, `tags`, and `advertised`. The server resolves `want-ref`
/// against its current state, so this is the authoritative OID when the advertised value was stale
/// (t5703 change-while-negotiating). Overriding `advertised` too is essential: the fetch caller
/// builds its ref-update map from the advertised list, so a stale advertised OID would otherwise win.
fn apply_wanted_refs_section(
    r: &mut impl Read,
    heads: &mut [LsRefEntry],
    tags: &mut [LsRefEntry],
    advertised: &mut [LsRefEntry],
) -> Result<()> {
    loop {
        match pkt_line::read_packet(r)? {
            None | Some(pkt_line::Packet::Flush) | Some(pkt_line::Packet::Delim) => return Ok(()),
            Some(pkt_line::Packet::Data(line)) => {
                let line = line.trim_end();
                if let Some((hex, name)) = line.split_once(' ') {
                    if let Ok(oid) = ObjectId::from_hex(hex.trim()) {
                        let name = name.trim();
                        for e in heads
                            .iter_mut()
                            .chain(tags.iter_mut())
                            .chain(advertised.iter_mut())
                        {
                            if e.name == name {
                                e.oid = oid;
                            }
                        }
                    }
                }
            }
            Some(_) => {}
        }
    }
}

/// Ref advertisement from protocol v2 `ls-refs`.
#[derive(Clone, Debug)]
pub struct LsRefEntry {
    pub name: String,
    pub oid: ObjectId,
}

/// Wire options for HTTP smart fetch requests.
#[derive(Clone, Debug, Default)]
pub struct HttpFetchOptions {
    /// When set, restrict emitted `have` lines to commits reachable from these tips
    /// (`git fetch --negotiation-tip`, peeled to commits). An empty slice suppresses haves.
    pub negotiation_tip_oids: Option<Vec<ObjectId>>,
    /// Absolute depth requested by `--depth`.
    pub depth: Option<usize>,
    /// Relative deepening requested by `--deepen`.
    pub deepen: Option<usize>,
    /// Date boundary requested by `--shallow-since`.
    pub shallow_since: Option<String>,
    /// Exclusion revisions requested by `--shallow-exclude`.
    pub shallow_exclude: Vec<String>,
    /// Partial-clone filter specification requested by `--filter`.
    pub filter_spec: Option<String>,
    /// Request full-object transfer without have/common negotiation (`--refetch`).
    pub refetch: bool,
    /// Suppress protocol-v2 bundle-uri discovery because the caller supplied an explicit URI.
    pub bundle_uri_override: bool,
    /// When true, omit `no-progress` from fetch capabilities so the server may emit side-band progress.
    pub show_progress: bool,
}

fn requested_depth(opts: &HttpFetchOptions) -> Option<usize> {
    opts.depth.or(opts.deepen).filter(|d| *d > 0)
}

fn build_fetch_caps_v2(options: &HttpFetchOptions) -> String {
    let mut parts = vec![
        "thin-pack".to_string(),
        "ofs-delta".to_string(),
        "side-band-64k".to_string(),
    ];
    if !options.show_progress {
        parts.push("no-progress".to_string());
    }
    parts.push("wait-for-done".to_string());
    parts.join(" ")
}

/// Convert a `--shallow-since`/`--deepen-since` date argument to the wire `deepen-since` value.
///
/// Git's `fetch-pack` runs `approxidate()` on the user-supplied date and sends the resulting Unix
/// timestamp as a bare integer (`upload-pack` parses it with `parse_timestamp` and rejects trailing
/// garbage). Sending the raw string (e.g. `"200000000 +0700"`) makes a real `upload-pack` die with
/// no output, so the shallow-since fetch silently transfers nothing (t5539 "fetch shallow since").
fn deepen_since_wire_value(since: &str) -> String {
    let since = since.trim();
    let ts = grit_lib::git_date::approx::approxidate_careful(since, None);
    ts.to_string()
}

fn append_fetch_request_extensions_v0_v1(
    req: &mut Vec<u8>,
    caps: &std::collections::HashSet<String>,
    options: &HttpFetchOptions,
    local_shallow_oids: &[ObjectId],
) -> Result<()> {
    for oid in local_shallow_oids {
        pkt_line::write_line_to_vec(req, &format!("shallow {}", oid.to_hex()))?;
    }
    if let Some(depth) = requested_depth(options) {
        pkt_line::write_line_to_vec(req, &format!("deepen {depth}"))?;
    }
    if let Some(since) = options.shallow_since.as_deref() {
        if caps.contains("deepen-since") {
            let value = deepen_since_wire_value(since);
            pkt_line::write_line_to_vec(req, &format!("deepen-since {value}"))?;
        }
    }
    if caps.contains("deepen-not") {
        for excl in &options.shallow_exclude {
            let excl = excl.trim();
            if excl.is_empty() {
                continue;
            }
            pkt_line::write_line_to_vec(req, &format!("deepen-not {excl}"))?;
        }
    }
    if caps.contains("filter") {
        if let Some(filter_spec) = options.filter_spec.as_deref() {
            let filter_spec = filter_spec.trim();
            if !filter_spec.is_empty() {
                // Send the canonical/expanded filter spec (Git's
                // `expand_list_objects_filter_spec`, e.g. `blob:limit=1k` -> `blob:limit=1024`).
                let expanded = grit_lib::rev_list::expand_object_filter_for_protocol(filter_spec)
                    .unwrap_or_else(|_| filter_spec.to_owned());
                pkt_line::write_line_to_vec(req, &format!("filter {expanded}"))?;
            }
        }
    }
    Ok(())
}

use grit_lib::protocol_v2::fetch_features as v2_fetch_features;

/// Split a v2-over-HTTP fetch's wants into `(want_ref_names, plain_want_oids)` when the server
/// advertised `ref-in-want`.
///
/// Mirrors `fetch-pack.c add_wants`: a sought ref that resolves to a named advertised ref is
/// requested as `want-ref <name>` (the server re-resolves it against its current state, which is
/// what handles the advertised ref changing mid-negotiation); an exact-OID source stays a plain
/// `want <oid>`. For the default/configured wildcard fetch (`refspecs` empty), every advertised
/// head/tag we want becomes a `want-ref`.
fn http_want_refs_and_plain_wants(
    advertised: &[LsRefEntry],
    refspecs: &[String],
    wants: &[ObjectId],
) -> (Vec<String>, Vec<ObjectId>) {
    let want_set: HashSet<ObjectId> = wants.iter().copied().collect();

    // Names of exact-OID refspec sources must NOT be turned into want-ref (they have no ref name).
    let exact_oids: HashSet<ObjectId> = refspecs
        .iter()
        .filter(|s| !s.starts_with('^'))
        .filter_map(|s| {
            let clean = s.strip_prefix('+').unwrap_or(s);
            let src = clean.split_once(':').map(|(a, _)| a).unwrap_or(clean);
            ObjectId::from_hex(src).ok()
        })
        .collect();

    let mut want_refs: Vec<String> = Vec::new();
    let mut covered: HashSet<ObjectId> = HashSet::new();
    for e in advertised {
        if !want_set.contains(&e.oid) || exact_oids.contains(&e.oid) {
            continue;
        }
        // Only request real, fetchable refs by name (heads, tags, and explicit refs/*). Skip the
        // synthetic `HEAD` advertisement and pseudo-refs.
        if !(e.name.starts_with("refs/heads/") || e.name.starts_with("refs/tags/")) {
            continue;
        }
        if want_refs.iter().any(|w| w == &e.name) {
            continue;
        }
        want_refs.push(e.name.clone());
        covered.insert(e.oid);
    }

    let plain_wants: Vec<ObjectId> = wants
        .iter()
        .copied()
        .filter(|o| !covered.contains(o))
        .collect();
    (want_refs, plain_wants)
}

fn append_fetch_request_extensions_v2(
    req: &mut Vec<u8>,
    caps: &[String],
    options: &HttpFetchOptions,
    local_shallow_oids: &[ObjectId],
) -> Result<()> {
    let features = v2_fetch_features(caps);
    if features.contains("shallow") {
        for oid in local_shallow_oids {
            pkt_line::write_line_to_vec(req, &format!("shallow {}", oid.to_hex()))?;
        }
    }
    if let Some(depth) = requested_depth(options) {
        pkt_line::write_line_to_vec(req, &format!("deepen {depth}"))?;
    }
    if let Some(since) = options.shallow_since.as_deref() {
        if features.contains("deepen-since") || features.contains("shallow") {
            let value = deepen_since_wire_value(since);
            pkt_line::write_line_to_vec(req, &format!("deepen-since {value}"))?;
        }
    }
    if features.contains("deepen-not") || features.contains("shallow") {
        for excl in &options.shallow_exclude {
            let excl = excl.trim();
            if excl.is_empty() {
                continue;
            }
            pkt_line::write_line_to_vec(req, &format!("deepen-not {excl}"))?;
        }
    }
    if features.contains("filter") {
        if let Some(filter_spec) = options.filter_spec.as_deref() {
            let filter_spec = filter_spec.trim();
            if !filter_spec.is_empty() {
                // Send the canonical/expanded filter spec (Git's
                // `expand_list_objects_filter_spec`, e.g. `blob:limit=1k` -> `blob:limit=1024`).
                let expanded = grit_lib::rev_list::expand_object_filter_for_protocol(filter_spec)
                    .unwrap_or_else(|_| filter_spec.to_owned());
                pkt_line::write_line_to_vec(req, &format!("filter {expanded}"))?;
            }
        }
    }
    Ok(())
}

/// Run `ls-refs` over smart HTTP and return advertised refs.
pub fn http_ls_refs(
    repo_url: &str,
    client: &crate::http_client::HttpClientContext,
) -> Result<Vec<LsRefEntry>> {
    let original_base = repo_url.trim_end_matches('/');
    let mut refs_url = format!("{original_base}/info/refs");
    refs_url.push_str(if refs_url.contains('?') { "&" } else { "?" });
    refs_url.push_str(&format!("service={SERVICE}"));

    let (body, base) = http_get_discovery_with_base(
        client,
        &refs_url,
        original_base,
        client.git_protocol_header(),
    )?;
    let pkt_body = strip_v0_service_advertisement_if_present(&body)?;
    let (caps, object_format) = match discover_http_protocol(pkt_body)? {
        HttpDiscovery::V2 {
            caps,
            object_format,
        } => (caps, object_format),
        HttpDiscovery::V0V1 { advertised, .. } => return Ok(advertised),
    };

    let mut req = Vec::new();
    pkt_line::write_line_to_vec(&mut req, "command=ls-refs")?;
    pkt_line::write_line_to_vec(&mut req, &format!("object-format={object_format}"))?;
    for line in cap_lines_for_client_request(&caps) {
        pkt_line::write_line_to_vec(&mut req, &line)?;
    }
    pkt_line::write_delim(&mut req)?;
    pkt_line::write_line_to_vec(&mut req, "ref-prefix HEAD")?;
    pkt_line::write_line_to_vec(&mut req, "ref-prefix refs/heads/")?;
    pkt_line::write_line_to_vec(&mut req, "ref-prefix refs/tags/")?;
    pkt_line::write_line_to_vec(&mut req, "peel")?;
    pkt_line::write_line_to_vec(&mut req, "symrefs")?;
    pkt_line::write_flush(&mut req)?;

    let post_url = format!("{base}/{SERVICE}");
    let resp = http_post_discovery(
        client,
        &post_url,
        &format!("application/x-{SERVICE}-request"),
        &format!("application/x-{SERVICE}-result"),
        &req,
        client.git_protocol_header(),
    )?;

    parse_ls_refs_v2_response(&resp.into_vec()?)
}

/// Perform HTTP smart protocol-v2 negotiation-only common-base discovery.
///
/// Returns local commit IDs that are common with the remote and reachable from the provided
/// negotiation tips. This mirrors fetch's negotiate-only behavior without downloading pack data.
pub fn http_negotiate_only_common(
    local_git_dir: &Path,
    repo_url: &str,
    negotiation_tips: &[ObjectId],
    client: &crate::http_client::HttpClientContext,
) -> Result<Vec<ObjectId>> {
    let original_base = repo_url.trim_end_matches('/');
    let mut refs_url = format!("{original_base}/info/refs");
    refs_url.push_str(if refs_url.contains('?') { "&" } else { "?" });
    refs_url.push_str(&format!("service={SERVICE}"));

    let (body, base) = http_get_discovery_with_base(
        client,
        &refs_url,
        original_base,
        client.git_protocol_header(),
    )?;
    let pkt_body = strip_v0_service_advertisement_if_present(&body)?;
    let (caps, object_format) = match discover_http_protocol(pkt_body)? {
        HttpDiscovery::V2 {
            caps,
            object_format,
        } => (caps, object_format),
        HttpDiscovery::V0V1 { .. } => bail!("negotiate-only requires protocol v2"),
    };

    let features = v2_fetch_features(&caps);
    if !features.contains("wait-for-done") {
        bail!("server does not support wait-for-done");
    }

    let mut req = Vec::new();
    pkt_line::write_line_to_vec(&mut req, "command=ls-refs")?;
    pkt_line::write_line_to_vec(&mut req, &format!("object-format={object_format}"))?;
    for line in cap_lines_for_client_request(&caps) {
        pkt_line::write_line_to_vec(&mut req, &line)?;
    }
    pkt_line::write_delim(&mut req)?;
    pkt_line::write_line_to_vec(&mut req, "ref-prefix HEAD")?;
    pkt_line::write_line_to_vec(&mut req, "ref-prefix refs/heads/")?;
    pkt_line::write_line_to_vec(&mut req, "ref-prefix refs/tags/")?;
    pkt_line::write_line_to_vec(&mut req, "peel")?;
    pkt_line::write_line_to_vec(&mut req, "symrefs")?;
    pkt_line::write_flush(&mut req)?;

    let post_url = format!("{base}/{SERVICE}");
    let resp = http_post_discovery(
        client,
        &post_url,
        &format!("application/x-{SERVICE}-request"),
        &format!("application/x-{SERVICE}-result"),
        &req,
        client.git_protocol_header(),
    )?;
    let advertised = parse_ls_refs_v2_response(&resp.into_vec()?)?;

    let local_repo = Repository::open(local_git_dir, None)
        .with_context(|| format!("open repository {}", local_git_dir.display()))?;
    let mut remote_oids: Vec<ObjectId> = advertised.iter().map(|e| e.oid).collect();
    remote_oids.sort_by_key(|o| o.to_hex());
    remote_oids.dedup();

    let mut commons = Vec::new();
    for tip in negotiation_tips {
        if local_repo.odb.read(tip).is_err() {
            continue;
        }
        for remote_oid in &remote_oids {
            if local_repo.odb.read(remote_oid).is_err() {
                continue;
            }
            if let Ok(mut bases) =
                merge_base::merge_bases_first_vs_rest(&local_repo, *tip, &[*remote_oid])
            {
                commons.append(&mut bases);
            }
        }
    }
    commons.sort_by_key(|o| o.to_hex());
    commons.dedup();
    Ok(commons)
}

fn parse_ls_refs_v2_response(data: &[u8]) -> Result<Vec<LsRefEntry>> {
    let mut cur = Cursor::new(data);
    let mut out = Vec::new();
    loop {
        let pkt = match pkt_line::read_packet(&mut cur)? {
            None => break,
            Some(pkt_line::Packet::Flush) => break,
            Some(pkt_line::Packet::Data(line)) => line,
            Some(other) => bail!("unexpected ls-refs packet: {other:?}"),
        };
        let (oid_hex, rest) = pkt
            .split_once(' ')
            .ok_or_else(|| anyhow::anyhow!("bad ls-refs line: {pkt}"))?;
        let oid = ObjectId::from_hex(oid_hex.trim())?;
        let name = rest.split_whitespace().next().unwrap_or(rest).to_string();
        if name.is_empty() {
            continue;
        }
        if !grit_lib::refs::is_valid_ls_refs_advertised_name(&name) {
            continue;
        }
        out.push(LsRefEntry { name, oid });
    }
    Ok(out)
}

fn collect_wants_from_advertised(
    advertised: &[LsRefEntry],
    refspecs: &[String],
) -> Result<Vec<ObjectId>> {
    if refspecs.is_empty() {
        let mut wants = Vec::new();
        for e in advertised {
            if e.name.starts_with("refs/heads/") || e.name.starts_with("refs/tags/") {
                wants.push(e.oid);
            }
        }
        wants.sort_by_key(|o| o.to_hex());
        wants.dedup();
        return Ok(wants);
    }
    let mut wants = Vec::new();
    let negative_patterns: Vec<&str> = refspecs
        .iter()
        .filter_map(|s| s.strip_prefix('^'))
        .collect();
    let is_excluded = |refname: &str| -> bool {
        negative_patterns
            .iter()
            .any(|pat| match_glob_pattern(pat, refname).is_some() || *pat == refname)
    };
    for spec in refspecs {
        if spec.starts_with('^') {
            continue;
        }
        let spec_clean = spec.strip_prefix('+').unwrap_or(spec);
        let src = spec_clean
            .split_once(':')
            .map(|(a, _)| a)
            .unwrap_or(spec_clean);
        if let Ok(oid) = ObjectId::from_hex(src) {
            wants.push(oid);
            continue;
        }
        if src.contains('*') {
            for e in advertised {
                if is_excluded(&e.name) {
                    continue;
                }
                if match_glob_pattern(src, &e.name).is_some() {
                    wants.push(e.oid);
                }
            }
            continue;
        }
        let remote_ref =
            resolve_advertised_ref_for_fetch_src(src, advertised).unwrap_or_else(|| {
                if src.starts_with("refs/") {
                    src.to_string()
                } else {
                    format!("refs/heads/{src}")
                }
            });
        if is_excluded(&remote_ref) {
            continue;
        }
        let oid = advertised
            .iter()
            .find(|e| e.name == remote_ref)
            .map(|e| e.oid)
            .or_else(|| {
                let tag_ref = format!("refs/tags/{src}");
                if is_excluded(&tag_ref) {
                    return None;
                }
                advertised.iter().find(|e| e.name == tag_ref).map(|e| e.oid)
            })
            .with_context(|| format!("could not find remote ref '{remote_ref}'"))?;
        wants.push(oid);
    }
    wants.sort_by_key(|o| o.to_hex());
    wants.dedup();
    Ok(wants)
}

fn resolve_advertised_ref_for_fetch_src(src: &str, advertised: &[LsRefEntry]) -> Option<String> {
    if src.is_empty() || src == "HEAD" {
        return Some("HEAD".to_string());
    }
    if src.starts_with("refs/") {
        return Some(src.to_string());
    }
    let candidates = [
        format!("refs/{src}"),
        format!("refs/tags/{src}"),
        format!("refs/heads/{src}"),
        format!("refs/remotes/{src}"),
        format!("refs/remotes/{src}/HEAD"),
    ];
    candidates
        .into_iter()
        .find(|cand| advertised.iter().any(|e| e.name == *cand))
}

fn has_fetch_request_extensions(options: &HttpFetchOptions) -> bool {
    requested_depth(options).is_some()
        || options
            .shallow_since
            .as_deref()
            .is_some_and(|v| !v.trim().is_empty())
        || options.shallow_exclude.iter().any(|v| !v.trim().is_empty())
        || options
            .filter_spec
            .as_deref()
            .is_some_and(|v| !v.trim().is_empty())
}

fn read_local_shallow_oids(local_git_dir: &Path) -> Result<Vec<ObjectId>> {
    let shallow_path = local_git_dir.join("shallow");
    if !shallow_path.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for line in std::fs::read_to_string(&shallow_path)?
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        if let Ok(oid) = ObjectId::from_hex(line) {
            out.push(oid);
        }
    }
    Ok(out)
}

fn apply_shallow_updates(
    local_git_dir: &Path,
    shallow: &[ObjectId],
    unshallow: &[ObjectId],
) -> Result<()> {
    if shallow.is_empty() && unshallow.is_empty() {
        return Ok(());
    }
    let mut boundaries: HashSet<ObjectId> = read_local_shallow_oids(local_git_dir)?
        .into_iter()
        .collect();
    for oid in shallow {
        boundaries.insert(*oid);
    }
    for oid in unshallow {
        boundaries.remove(oid);
    }

    let shallow_path = local_git_dir.join("shallow");
    if boundaries.is_empty() {
        let _ = std::fs::remove_file(shallow_path);
        return Ok(());
    }

    let mut lines = boundaries
        .into_iter()
        .map(|oid| oid.to_hex())
        .collect::<Vec<_>>();
    lines.sort();
    let mut contents = lines.join("\n");
    contents.push('\n');
    std::fs::write(&shallow_path, contents)
        .with_context(|| format!("write {}", shallow_path.display()))?;
    Ok(())
}

fn read_shallow_info_section(r: &mut impl Read) -> Result<(Vec<ObjectId>, Vec<ObjectId>)> {
    let mut shallow = Vec::new();
    let mut unshallow = Vec::new();
    loop {
        match pkt_line::read_packet(r)? {
            // A protocol-v2 `fetch` response separates its sections with a delim packet (`0001`),
            // not a flush. The `shallow-info` section therefore ends at the delim that precedes the
            // following `packfile` section. Treating the delim as a no-op (`continue`) would consume
            // the `packfile` header and the entire pack as if they were shallow-info lines, leaving
            // nothing for the caller to unpack — the fetched objects would silently never be stored
            // (t5537 "shallow fetches check connectivity before writing shallow file").
            None | Some(pkt_line::Packet::Flush) | Some(pkt_line::Packet::Delim) => break,
            Some(pkt_line::Packet::Data(line)) => {
                if let Some(rest) = line.strip_prefix("shallow ") {
                    let oid = ObjectId::from_hex(rest.trim())
                        .with_context(|| format!("parse shallow oid {}", rest.trim()))?;
                    shallow.push(oid);
                } else if let Some(rest) = line.strip_prefix("unshallow ") {
                    let oid = ObjectId::from_hex(rest.trim())
                        .with_context(|| format!("parse unshallow oid {}", rest.trim()))?;
                    unshallow.push(oid);
                }
            }
            Some(other) => bail!("unexpected shallow-info packet: {other:?}"),
        }
    }
    Ok((shallow, unshallow))
}

fn match_glob_pattern<'a>(pattern: &str, refname: &'a str) -> Option<&'a str> {
    if let Some(star_pos) = pattern.find('*') {
        let prefix = &pattern[..star_pos];
        let suffix = &pattern[star_pos + 1..];
        if refname.starts_with(prefix)
            && refname.ends_with(suffix)
            && refname.len() >= prefix.len() + suffix.len()
        {
            Some(&refname[prefix.len()..refname.len() - suffix.len()])
        } else {
            None
        }
    } else if pattern == refname {
        Some(refname)
    } else {
        None
    }
}

fn build_fetch_caps_v0(
    caps: &std::collections::HashSet<String>,
    options: &HttpFetchOptions,
) -> String {
    let mut enabled = Vec::new();
    let multi_ack_detailed = caps.contains("multi_ack_detailed");
    if multi_ack_detailed {
        enabled.push("multi_ack_detailed");
    }
    // `no-done` (mirrors `fetch-pack.c`: requested only alongside `multi_ack_detailed`) lets the
    // stateless-RPC server stream the pack right after `ACK <oid> ready`, so the client never has
    // to send `done` (t5539 "no shallow lines after receiving ACK ready").
    if multi_ack_detailed && caps.contains("no-done") {
        enabled.push("no-done");
    }
    for want in ["side-band-64k", "thin-pack", "include-tag", "ofs-delta"] {
        if caps.contains(want) {
            enabled.push(want);
        }
    }
    if !options.show_progress && caps.contains("no-progress") {
        enabled.push("no-progress");
    }
    if enabled.is_empty() {
        String::new()
    } else {
        format!(" {}", enabled.join(" "))
    }
}

/// Emit one `GIT_TRACE_PACKET` line with the `fetch-pack` identity for v0/v1 HTTP negotiation.
///
/// Git's HTTP fetch drives the `fetch-pack` machinery (`packet_trace_identity("fetch-pack")`), so
/// the negotiation pkt-lines are traced as `packet:   fetch-pack> …` / `fetch-pack< …`. Tests grep
/// these (t5539 asserts `fetch-pack< ACK .* ready` is present and `fetch-pack> done` is absent).
fn trace_fetch_pack_packet(direction: char, payload: &str) {
    crate::wire_trace::trace_packet_line_ident("fetch-pack", direction, payload);
}

#[allow(clippy::too_many_arguments)]
fn fetch_pack_v0_v1_stateless_http(
    local_git_dir: &Path,
    base: &str,
    advertised: &[LsRefEntry],
    refspecs: &[String],
    caps: &std::collections::HashSet<String>,
    filter_active: bool,
    options: &HttpFetchOptions,
    client: &crate::http_client::HttpClientContext,
) -> Result<HttpFetchResult> {
    let object_format = caps
        .iter()
        .find_map(|c| c.strip_prefix("object-format="))
        .unwrap_or("sha1")
        .to_string();
    let wants = collect_wants_from_advertised(advertised, refspecs)?;
    let remote_heads: Vec<_> = advertised
        .iter()
        .filter(|e| e.name.starts_with("refs/heads/"))
        .cloned()
        .collect();
    let remote_tags: Vec<_> = advertised
        .iter()
        .filter(|e| e.name.starts_with("refs/tags/"))
        .cloned()
        .collect();
    let all_advertised = advertised.to_vec();
    if wants.is_empty() {
        // Empty repository: nothing to fetch, but report the advertised refs and object format
        // so the clone caller can record SHA-256 (`t5551` empty SHA-256 clone over protocol v0).
        return Ok(HttpFetchResult {
            heads: remote_heads,
            tags: remote_tags,
            all_advertised,
            object_format,
        });
    }
    if !options.refetch && !has_fetch_request_extensions(options) {
        let repo = Repository::open(local_git_dir, None)
            .with_context(|| format!("open {}", local_git_dir.display()))?;
        let all_wants_local = wants.iter().all(|oid| repo.odb.read(oid).is_ok());
        if all_wants_local {
            return Ok(HttpFetchResult {
                heads: remote_heads,
                tags: remote_tags,
                all_advertised,
                object_format,
            });
        }
    }

    let fetch_caps = build_fetch_caps_v0(caps, options);
    let want_set: HashSet<ObjectId> = wants.iter().copied().collect();
    let mut negotiator = if options.refetch {
        None
    } else {
        let local_repo = Repository::open(local_git_dir, None)
            .with_context(|| format!("open {}", local_git_dir.display()))?;
        let mut negotiator = SkippingNegotiator::new(local_repo);

        for w in &wants {
            if negotiator.repo().odb.read(w).is_ok() {
                negotiator.add_tip(*w)?;
            }
        }
        let mut tips: Vec<ObjectId> = Vec::new();
        for prefix in ["refs/heads/", "refs/tags/"] {
            if let Ok(entries) = refs::list_refs(local_git_dir, prefix) {
                for (name, oid) in entries {
                    let tip = if let Ok(resolved) = resolve_revision(negotiator.repo(), &name) {
                        resolved
                    } else {
                        oid
                    };
                    if negotiator.repo().odb.read(&tip).is_err() {
                        continue;
                    }
                    tips.push(tip);
                }
            }
        }
        if let Ok(h) = refs::resolve_ref(local_git_dir, "HEAD") {
            if negotiator.repo().odb.read(&h).is_ok() {
                tips.push(h);
            }
        }
        tips.sort_by_key(|o| o.to_hex());
        tips.dedup();
        for t in tips {
            if want_set.contains(&t) {
                continue;
            }
            if negotiator.repo().odb.read(&t).is_err() {
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
        Some(negotiator)
    };

    let local_shallow_oids = read_local_shallow_oids(local_git_dir)?;
    let sideband = caps.contains("side-band-64k");
    let multi_ack_detailed = caps.contains("multi_ack_detailed");
    let no_done = multi_ack_detailed && caps.contains("no-done");
    let depth_requested = requested_depth(options).is_some()
        || options.shallow_since.is_some()
        || !options.shallow_exclude.is_empty()
        || !local_shallow_oids.is_empty();
    let post_url = format!("{base}/{SERVICE}");

    // Persistent request prefix replayed on every stateless RPC: the `want` lines (capabilities on
    // the first), shallow/deepen extensions, and the terminating flush. Mirrors `fetch-pack.c`
    // `find_common`, where `state_len` marks the bytes re-sent each round.
    let mut state = Vec::new();
    let first = wants[0];
    let want_first = format!("want {}{}", first.to_hex(), fetch_caps);
    pkt_line::write_line_to_vec(&mut state, &want_first)?;
    trace_fetch_pack_packet('>', &want_first);
    for w in wants.iter().skip(1) {
        let line = format!("want {}", w.to_hex());
        pkt_line::write_line_to_vec(&mut state, &line)?;
        trace_fetch_pack_packet('>', &line);
    }
    append_fetch_request_extensions_v0_v1(&mut state, caps, options, &local_shallow_oids)?;
    pkt_line::write_flush(&mut state)?;

    let mut pack_path: Option<PathBuf> = None;
    let progress = SidebandProgress {
        show: options.show_progress,
    };
    let mut got_ready = false;
    let mut got_pack = false;
    let mut shallow_applied = false;

    // Multi-round stateless negotiation (mirrors `fetch-pack.c` `find_common` for `stateless_rpc`):
    // batch `have` lines, POST `state + haves + flush`, read ACK responses, and replay still-uncommon
    // haves on the next RPC. When the server replies `ACK <oid> ready` (and `no-done` was negotiated)
    // it streams the pack in that same response, so the client never sends `done` (t5539 test 3).
    const INITIAL_FLUSH: usize = 16;
    let mut count: usize = 0;
    let mut flush_at: usize = INITIAL_FLUSH;
    if let Some(negotiator) = negotiator.as_mut() {
        let mut round = Vec::new();
        while let Some(oid) = negotiator.next_have()? {
            let line = format!("have {}", oid.to_hex());
            pkt_line::write_line_to_vec(&mut round, &line)?;
            trace_fetch_pack_packet('>', &line);
            count += 1;
            if count < flush_at {
                continue;
            }
            flush_at = next_flush_v0_stateless(count);

            let mut req = state.clone();
            req.extend_from_slice(&round);
            pkt_line::write_flush(&mut req)?;
            round.clear();

            let mut resp = http_post(
                client,
                &post_url,
                &format!("application/x-{SERVICE}-request"),
                &format!("application/x-{SERVICE}-result"),
                &req,
            )?;
            let mut reader = resp.reader()?;
            let round_result = read_v0_stateless_response_reader(
                &mut reader,
                local_git_dir,
                sideband,
                depth_requested,
                progress,
                &mut pack_path,
            )?;
            if depth_requested && !shallow_applied {
                apply_shallow_updates(
                    local_git_dir,
                    &round_result.shallow,
                    &round_result.unshallow,
                )?;
                shallow_applied = true;
            }
            for ack in &round_result.acks {
                // `ACK <oid> common/ready/continue` update the negotiator (a bare `ACK <oid>`
                // ends a round). For stateless RPC, an `ACK common` whose commit was not already
                // marked common must have its `have` replayed on the next RPC so the server keeps
                // it in the common set (`fetch-pack.c` ACK_common replay).
                if matches!(ack.kind, V0AckKind::Bare) {
                    continue;
                }
                let was_common = negotiator.ack(ack.oid)?;
                if matches!(ack.kind, V0AckKind::Common) && !was_common {
                    let line = format!("have {}", ack.oid.to_hex());
                    pkt_line::write_line_to_vec(&mut state, &line)?;
                }
                if matches!(ack.kind, V0AckKind::Ready) {
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
    }

    // Send `done` unless the server became `ready` and `no-done` was negotiated. In the no-done case
    // the pack already arrived alongside `ACK <oid> ready`; otherwise issue a final RPC ending in
    // `done` to trigger pack generation (`fetch-pack.c`: `if (!got_ready || !no_done) send done`).
    if !(got_pack || got_ready && no_done) {
        let mut req = state.clone();
        let done = "done";
        pkt_line::write_line_to_vec(&mut req, done)?;
        trace_fetch_pack_packet('>', done);
        pkt_line::write_flush(&mut req)?;

        let mut resp = http_post(
            client,
            &post_url,
            &format!("application/x-{SERVICE}-request"),
            &format!("application/x-{SERVICE}-result"),
            &req,
        )?;
        let mut reader = resp.reader()?;
        let round_result = read_v0_stateless_response_reader(
            &mut reader,
            local_git_dir,
            sideband,
            depth_requested,
            progress,
            &mut pack_path,
        )?;
        if depth_requested && !shallow_applied {
            apply_shallow_updates(
                local_git_dir,
                &round_result.shallow,
                &round_result.unshallow,
            )?;
            shallow_applied = true;
        }
        got_pack = round_result.got_pack;
    }

    let _ = (shallow_applied, got_pack);

    if let Some(path) = pack_path.take() {
        crate::fetch_transport::unpack_upload_pack_file(local_git_dir, &path, filter_active)
            .context("ingest fetched pack")?;
        let _ = std::fs::remove_file(path);
    }

    Ok(HttpFetchResult {
        heads: remote_heads,
        tags: remote_tags,
        all_advertised,
        object_format,
    })
}

/// Next stateless-RPC `have` batch size (mirrors `fetch-pack.c` `next_flush` with `stateless_rpc`).
fn next_flush_v0_stateless(count: usize) -> usize {
    const LARGE_FLUSH: usize = 16384;
    if count < LARGE_FLUSH {
        count * 2
    } else {
        count * 11 / 10
    }
}

/// Kind of `ACK` status suffix in a v0 negotiation response.
#[derive(Clone, Copy, PartialEq, Eq)]
enum V0AckKind {
    /// `ACK <oid>` with no status (ends a round / post-`done`).
    Bare,
    Common,
    Continue,
    Ready,
}

/// One parsed `ACK <oid> [status]` line.
struct V0Ack {
    oid: ObjectId,
    kind: V0AckKind,
}

/// Parsed result of one stateless-RPC negotiation response.
struct V0StatelessResponse {
    shallow: Vec<ObjectId>,
    unshallow: Vec<ObjectId>,
    acks: Vec<V0Ack>,
    got_pack: bool,
}

/// Parse one v0 stateless-RPC `git-upload-pack` response.
///
/// The response begins with an optional shallow-info section (when a depth/since/exclude was
/// requested), terminated by a flush; then plain `ACK`/`NAK` negotiation pkt-lines; then, if the
/// server is generating a pack, the packfile (side-band-multiplexed when `side-band-64k` was
/// negotiated). Pack bytes are spooled to a temp file under `objects/pack/` when present.
/// Each negotiation line is traced as `fetch-pack< …` so tests grepping `GIT_TRACE_PACKET`
/// match (t5539).
fn read_v0_stateless_response(
    resp: &[u8],
    git_dir: &Path,
    sideband: bool,
    expect_shallow: bool,
    progress: SidebandProgress,
    pack_path: &mut Option<PathBuf>,
) -> Result<V0StatelessResponse> {
    let mut cur = Cursor::new(resp);
    read_v0_stateless_response_reader(
        &mut cur,
        git_dir,
        sideband,
        expect_shallow,
        progress,
        pack_path,
    )
}

fn read_v0_stateless_response_reader(
    resp: &mut impl Read,
    git_dir: &Path,
    sideband: bool,
    expect_shallow: bool,
    progress: SidebandProgress,
    pack_path: &mut Option<PathBuf>,
) -> Result<V0StatelessResponse> {
    let mut cur = BufReader::new(resp);
    let mut shallow = Vec::new();
    let mut unshallow = Vec::new();
    let mut acks = Vec::new();
    let mut got_pack = false;

    let mut in_shallow = expect_shallow;
    while let Some(payload) = crate::fetch_transport::read_pkt_payload_raw(&mut cur)? {
        if payload.is_empty() {
            if in_shallow {
                in_shallow = false;
            }
            continue;
        }
        let is_pack =
            (sideband && payload.first() == Some(&1) && payload.get(1..5) == Some(b"PACK"))
                || payload.starts_with(b"PACK");
        if is_pack {
            got_pack = true;
            let pack_dir = git_dir.join("objects/pack");
            std::fs::create_dir_all(&pack_dir)?;
            let mut tmp =
                tempfile::NamedTempFile::new_in(&pack_dir).context("create temp pack file")?;
            if sideband {
                let data = payload.get(1..).unwrap_or(&payload);
                tmp.write_all(data)?;
                read_sideband_pack_to_writer(&mut cur, &mut tmp, progress)?;
            } else {
                tmp.write_all(&payload)?;
                std::io::copy(&mut cur, &mut tmp)?;
            }
            tmp.flush()?;
            let path = tmp.path().to_path_buf();
            tmp.persist(&path)
                .map_err(|e| anyhow::anyhow!("persist temp pack file: {}", e.error))?;
            *pack_path = Some(path);
            break;
        }
        let text = String::from_utf8_lossy(&payload);
        let line = text.trim_end_matches('\n');
        if in_shallow {
            if let Some(rest) = line.strip_prefix("shallow ") {
                trace_fetch_pack_packet('<', line);
                if let Ok(oid) = ObjectId::from_hex(rest.trim()) {
                    shallow.push(oid);
                }
                continue;
            }
            if let Some(rest) = line.strip_prefix("unshallow ") {
                trace_fetch_pack_packet('<', line);
                if let Ok(oid) = ObjectId::from_hex(rest.trim()) {
                    unshallow.push(oid);
                }
                continue;
            }
            in_shallow = false;
        }
        if let Some(err) = line.strip_prefix("ERR ") {
            bail!("remote upload-pack error: {err}");
        }
        trace_fetch_pack_packet('<', line);
        if line == "NAK" {
            continue;
        }
        if let Some(ack) = parse_v0_ack(line) {
            acks.push(ack);
        }
    }

    Ok(V0StatelessResponse {
        shallow,
        unshallow,
        acks,
        got_pack,
    })
}

/// Parse an `ACK <oid> [common|continue|ready]` line into a [`V0Ack`].
fn parse_v0_ack(line: &str) -> Option<V0Ack> {
    let rest = line.strip_prefix("ACK ")?;
    let hex = rest.split_whitespace().next()?;
    let oid = ObjectId::from_hex(hex).ok()?;
    let tail = rest.strip_prefix(hex).unwrap_or("").trim();
    let kind = if tail.contains("continue") {
        V0AckKind::Continue
    } else if tail.contains("common") {
        V0AckKind::Common
    } else if tail.contains("ready") {
        V0AckKind::Ready
    } else {
        V0AckKind::Bare
    };
    Some(V0Ack { oid, kind })
}

fn trace_clone_negotiation_line(line: &str) {
    crate::trace_packet::trace_packet_line(line.as_bytes());
}

fn fetch_negotiation_is_noop(local_git_dir: &Path) -> bool {
    grit_lib::config::ConfigSet::load(Some(local_git_dir), true)
        .ok()
        .and_then(|cfg| cfg.get("fetch.negotiationalgorithm"))
        .is_some_and(|value| value.eq_ignore_ascii_case("noop"))
}

fn peel_commit_oid_for_negotiation(repo: &Repository, oid: ObjectId) -> Result<Option<ObjectId>> {
    try_peel_to_commit_for_merge_base(repo, oid).map_err(|e| match e {
        grit_lib::error::Error::InvalidRef(msg) => anyhow::anyhow!(msg),
        other => other.into(),
    })
}

fn negotiation_tip_filter(
    repo: &Repository,
    negotiation_tip_oids: Option<&[ObjectId]>,
) -> Result<Option<HashSet<ObjectId>>> {
    let Some(tips) = negotiation_tip_oids else {
        return Ok(None);
    };
    let mut set = HashSet::new();
    for tip in tips {
        if let Some(peeled) = peel_commit_oid_for_negotiation(repo, *tip)? {
            set.insert(peeled);
        }
    }
    Ok(Some(set))
}

fn peeled_tip_passes_filter(
    repo: &Repository,
    tip: ObjectId,
    tip_filter: &Option<HashSet<ObjectId>>,
) -> Result<Option<ObjectId>> {
    if repo.odb.read(&tip).is_err() {
        return Ok(None);
    }
    let Some(peeled) = peel_commit_oid_for_negotiation(repo, tip)? else {
        return Ok(None);
    };
    if tip_filter
        .as_ref()
        .is_some_and(|filter| !filter.contains(&peeled))
    {
        return Ok(None);
    }
    Ok(Some(peeled))
}

/// Parse a protocol-v2 `command=fetch` response stream; returns true when a packfile section was consumed.
fn process_v2_fetch_response(
    reader: &mut impl Read,
    local_git_dir: &Path,
    filter_active: bool,
    progress: SidebandProgress,
    remote_heads: &mut [LsRefEntry],
    remote_tags: &mut [LsRefEntry],
    all_advertised: &mut [LsRefEntry],
) -> Result<bool> {
    let mut cur = BufReader::new(reader);
    loop {
        let pkt = match pkt_line::read_packet(&mut cur)? {
            None => break,
            Some(pkt_line::Packet::Flush) => break,
            Some(pkt_line::Packet::Delim) => continue,
            Some(pkt_line::Packet::Data(s)) => s,
            Some(other) => bail!("unexpected fetch response: {other:?}"),
        };
        if let Some(msg) = pkt.strip_prefix("ERR ") {
            bail!("fatal: remote error: {}", msg.trim_end());
        } else if pkt == "acknowledgments" {
            skip_to_flush(&mut cur)?;
        } else if pkt == "shallow-info" {
            let (shallow, unshallow) = read_shallow_info_section(&mut cur)?;
            apply_shallow_updates(local_git_dir, &shallow, &unshallow)?;
        } else if pkt == "wanted-refs" {
            apply_wanted_refs_section(&mut cur, remote_heads, remote_tags, all_advertised)?;
        } else if pkt == "packfile" {
            let pack_path = write_sideband_pack_to_temp(local_git_dir, &mut cur, progress)
                .context("receive packfile")?;
            crate::fetch_transport::unpack_upload_pack_file(
                local_git_dir,
                &pack_path,
                filter_active,
            )
            .context("ingest fetched pack")?;
            let _ = std::fs::remove_file(&pack_path);
            crate::trace_packet::trace_packet_line(b"clone> packfile negotiation complete");
            return Ok(true);
        }
    }
    Ok(false)
}

/// Fetch packfile via HTTP protocol v2 into `local_git_dir`, using the same skipping
/// negotiation idea as local upload-pack (initial have window, then `done`).
///
/// Result of an HTTP fetch/clone negotiation.
///
/// `object_format` is the hash algorithm advertised by the remote (`sha1` or `sha256`); the
/// clone caller persists it into the destination config so an empty SHA-256 repository is
/// cloned with the correct format (`t5551`).
pub struct HttpFetchResult {
    pub heads: Vec<LsRefEntry>,
    pub tags: Vec<LsRefEntry>,
    pub all_advertised: Vec<LsRefEntry>,
    pub object_format: String,
}

/// Returns the advertised refs and negotiated object format.
pub fn http_fetch_pack(
    local_git_dir: &Path,
    repo_url: &str,
    refspecs: &[String],
    filter_active: bool,
    options: &HttpFetchOptions,
    client: &crate::http_client::HttpClientContext,
) -> Result<HttpFetchResult> {
    trace_http_v0_v1_negotiated(client);
    let original_base = repo_url.trim_end_matches('/');
    let mut refs_url = format!("{original_base}/info/refs");
    refs_url.push_str(if refs_url.contains('?') { "&" } else { "?" });
    refs_url.push_str(&format!("service={SERVICE}"));

    let (body, base) = http_get_discovery_with_base(
        client,
        &refs_url,
        original_base,
        client.git_protocol_header(),
    )?;
    let base = base.as_str();
    let pkt_body = strip_v0_service_advertisement_if_present(&body)?;
    let discovery = discover_http_protocol(pkt_body)?;
    let (caps, object_format) = match discovery {
        HttpDiscovery::V2 {
            caps,
            object_format,
        } => (caps, object_format),
        HttpDiscovery::V0V1 { advertised, caps } => {
            return fetch_pack_v0_v1_stateless_http(
                local_git_dir,
                base,
                &advertised,
                refspecs,
                &caps,
                filter_active,
                options,
                client,
            )
        }
    };

    if !options.bundle_uri_override
        && crate::file_upload_pack_v2::server_advertises_bundle_uri(&caps)
        && crate::file_upload_pack_v2::transfer_bundle_uri_enabled()
    {
        let cap_send = crate::file_upload_pack_v2::cap_lines_for_bundle_request(&caps);
        let mut req = Vec::new();
        crate::file_upload_pack_v2::write_bundle_uri_command(&mut req, &cap_send)?;
        let post_url = format!("{base}/{SERVICE}");
        let resp = http_post_discovery(
            client,
            &post_url,
            &format!("application/x-{SERVICE}-request"),
            &format!("application/x-{SERVICE}-result"),
            &req,
            client.git_protocol_header(),
        )?;
        let body = resp.into_vec()?;
        let mut cur = Cursor::new(body.as_slice());
        crate::file_upload_pack_v2::drain_bundle_uri_response(&mut cur)?;
    }

    let advertised = {
        let mut req = Vec::new();
        pkt_line::write_line_to_vec(&mut req, "command=ls-refs")?;
        pkt_line::write_line_to_vec(&mut req, &format!("object-format={object_format}"))?;
        for line in cap_lines_for_client_request(&caps) {
            pkt_line::write_line_to_vec(&mut req, &line)?;
        }
        pkt_line::write_delim(&mut req)?;
        pkt_line::write_line_to_vec(&mut req, "ref-prefix HEAD")?;
        pkt_line::write_line_to_vec(&mut req, "ref-prefix refs/heads/")?;
        pkt_line::write_line_to_vec(&mut req, "ref-prefix refs/tags/")?;
        pkt_line::write_line_to_vec(&mut req, "peel")?;
        pkt_line::write_line_to_vec(&mut req, "symrefs")?;
        pkt_line::write_flush(&mut req)?;

        let post_url = format!("{base}/{SERVICE}");
        let resp = http_post_discovery(
            client,
            &post_url,
            &format!("application/x-{SERVICE}-request"),
            &format!("application/x-{SERVICE}-result"),
            &req,
            client.git_protocol_header(),
        )?;
        parse_ls_refs_v2_response(&resp.into_vec()?)?
    };

    let wants = collect_wants_from_advertised(&advertised, refspecs)?;
    // When the server advertised `ref-in-want`, request named refs by name so the server resolves
    // them against its current state (handles the advertised ref changing under us mid-negotiation;
    // t5703). Otherwise send only plain `want <oid>` lines.
    let ref_in_want = v2_fetch_features(&caps).contains("ref-in-want");
    let (want_refs, plain_wants) = if ref_in_want {
        http_want_refs_and_plain_wants(&advertised, refspecs, &wants)
    } else {
        (Vec::new(), wants.clone())
    };
    let mut all_advertised = advertised.clone();
    let mut remote_heads: Vec<_> = advertised
        .iter()
        .filter(|e| e.name.starts_with("refs/heads/"))
        .cloned()
        .collect();
    let mut remote_tags: Vec<_> = advertised
        .iter()
        .filter(|e| e.name.starts_with("refs/tags/"))
        .cloned()
        .collect();
    if wants.is_empty() && want_refs.is_empty() {
        // An empty repository (or a fully-excluded refspec) advertises no fetchable refs. The
        // clone/fetch still succeeds with nothing to download; the caller persists the
        // negotiated `object_format` so an empty SHA-256 repo is recorded as SHA-256 (`t5551`).
        return Ok(HttpFetchResult {
            heads: remote_heads,
            tags: remote_tags,
            all_advertised,
            object_format,
        });
    }

    let want_set: HashSet<ObjectId> = wants.iter().copied().collect();
    if !options.refetch && !has_fetch_request_extensions(options) {
        let repo = Repository::open(local_git_dir, None)
            .with_context(|| format!("open {}", local_git_dir.display()))?;
        let all_wants_local = wants.iter().all(|oid| repo.odb.read(oid).is_ok());
        if all_wants_local {
            return Ok(HttpFetchResult {
                heads: remote_heads,
                tags: remote_tags,
                all_advertised,
                object_format,
            });
        }
    }

    let suppress_haves = options
        .negotiation_tip_oids
        .as_ref()
        .is_some_and(|tips| tips.is_empty())
        || fetch_negotiation_is_noop(local_git_dir);

    let mut negotiator = if options.refetch {
        None
    } else {
        let local_repo = Repository::open(local_git_dir, None)
            .with_context(|| format!("open {}", local_git_dir.display()))?;
        let mut negotiator = SkippingNegotiator::new(local_repo);
        let tip_filter =
            negotiation_tip_filter(negotiator.repo(), options.negotiation_tip_oids.as_deref())?;

        if !suppress_haves {
            if let Ok(entries) = refs::list_refs(local_git_dir, "refs/bundles/") {
                for (name, oid) in entries {
                    let t = if let Ok(resolved) = resolve_revision(negotiator.repo(), &name) {
                        resolved
                    } else {
                        oid
                    };
                    if let Some(peeled) =
                        peeled_tip_passes_filter(negotiator.repo(), t, &tip_filter)?
                    {
                        negotiator.add_tip(peeled)?;
                    }
                }
            }

            for w in &wants {
                if negotiator.repo().odb.read(w).is_ok() {
                    if let Some(peeled) = peel_commit_oid_for_negotiation(negotiator.repo(), *w)? {
                        negotiator.add_tip(peeled)?;
                    }
                }
            }
            let mut tips: Vec<ObjectId> = Vec::new();
            for prefix in ["refs/heads/", "refs/tags/"] {
                if let Ok(entries) = refs::list_refs(local_git_dir, prefix) {
                    for (name, oid) in entries {
                        let tip = if let Ok(resolved) = resolve_revision(negotiator.repo(), &name) {
                            resolved
                        } else {
                            oid
                        };
                        if let Some(peeled) =
                            peeled_tip_passes_filter(negotiator.repo(), tip, &tip_filter)?
                        {
                            tips.push(peeled);
                        }
                    }
                }
            }
            if let Ok(h) = refs::resolve_ref(local_git_dir, "HEAD") {
                if let Some(peeled) = peeled_tip_passes_filter(negotiator.repo(), h, &tip_filter)? {
                    tips.push(peeled);
                }
            }
            for sym in ["HEAD", "MERGE_HEAD", "CHERRY_PICK_HEAD", "REVERT_HEAD"] {
                if let Ok(oid) = resolve_revision(negotiator.repo(), sym) {
                    if let Some(peeled) =
                        peeled_tip_passes_filter(negotiator.repo(), oid, &tip_filter)?
                    {
                        tips.push(peeled);
                    }
                }
            }
            tips.sort_by_key(|o| o.to_hex());
            tips.dedup();
            for t in tips {
                if want_set.contains(&t) {
                    continue;
                }
                if negotiator.repo().odb.read(&t).is_err() {
                    continue;
                }
                negotiator.add_tip(t)?;
            }
        }
        for e in &advertised {
            if want_set.contains(&e.oid) {
                continue;
            }
            if negotiator.repo().odb.read(&e.oid).is_ok() {
                negotiator.known_common(e.oid)?;
            }
        }
        Some(negotiator)
    };

    let post_url = format!("{base}/{SERVICE}");
    let cap_send = cap_lines_for_client_request(&caps);
    let fetch_caps = build_fetch_caps_v2(options);

    let mut pending_haves: Vec<ObjectId> = Vec::new();
    if let Some(negotiator) = negotiator.as_mut() {
        while let Some(oid) = negotiator.next_have()? {
            pending_haves.push(oid);
        }
    }

    let local_shallow_oids = read_local_shallow_oids(local_git_dir)?;
    let write_fetch_request = |include_done: bool| -> Result<Vec<u8>> {
        let mut req = Vec::new();
        pkt_line::write_line_to_vec(&mut req, "command=fetch")?;
        pkt_line::write_line_to_vec(&mut req, &format!("object-format={object_format}"))?;
        for line in &cap_send {
            pkt_line::write_line_to_vec(&mut req, line)?;
        }
        pkt_line::write_delim(&mut req)?;
        let mut first_line = true;
        for w in &plain_wants {
            // Trace the sent `want` line (matches Git's `packet: fetch> want <oid>`); tests grep the
            // packet trace to assert which objects were requested (t5616 REF_DELTA lazy fetch).
            crate::trace_packet::trace_packet_git('>', &format!("want {}", w.to_hex()));
            // Carry the fetch capabilities on the first request line (the server reads the leading
            // OID and ignores trailing text), matching the existing v2-over-HTTP framing.
            if first_line {
                pkt_line::write_line_to_vec(
                    &mut req,
                    &format!("want {} {}", w.to_hex(), fetch_caps),
                )?;
                first_line = false;
            } else {
                pkt_line::write_line_to_vec(&mut req, &format!("want {}", w.to_hex()))?;
            }
        }
        // `want-ref <name>` lines (ref-in-want). Sent clean — no trailing caps, which would corrupt
        // the refname. The recognized fetch features (thin-pack/ofs-delta/no-progress) are emitted
        // as standalone argument lines when no plain `want` line carried them; v2 always streams the
        // pack in side-band-64k regardless.
        if first_line && !want_refs.is_empty() {
            for feat in ["thin-pack", "ofs-delta"] {
                pkt_line::write_line_to_vec(&mut req, feat)?;
            }
            if !options.show_progress {
                pkt_line::write_line_to_vec(&mut req, "no-progress")?;
            }
        }
        for name in &want_refs {
            crate::trace_packet::trace_packet_git('>', &format!("want-ref {name}"));
            pkt_line::write_line_to_vec(&mut req, &format!("want-ref {name}"))?;
        }
        append_fetch_request_extensions_v2(&mut req, &caps, options, &local_shallow_oids)?;
        let trace_label = crate::trace_packet::negotiation_packet_label();
        for h in &pending_haves {
            let trace = format!("{trace_label}> have {}", h.to_hex());
            trace_clone_negotiation_line(&trace);
            pkt_line::write_line_to_vec(&mut req, &format!("have {}", h.to_hex()))?;
        }
        if include_done {
            pkt_line::write_line_to_vec(&mut req, "done")?;
            trace_clone_negotiation_line(&format!("{trace_label}> done"));
        }
        pkt_line::write_flush(&mut req)?;
        Ok(req)
    };

    let progress = SidebandProgress {
        show: options.show_progress,
    };

    if !pending_haves.is_empty() {
        let req = write_fetch_request(false)?;
        let mut resp = http_post(
            client,
            &post_url,
            &format!("application/x-{SERVICE}-request"),
            &format!("application/x-{SERVICE}-result"),
            &req,
        )?;
        let mut reader = resp.reader()?;
        if process_v2_fetch_response(
            &mut reader,
            local_git_dir,
            filter_active,
            progress,
            &mut remote_heads,
            &mut remote_tags,
            &mut all_advertised,
        )? {
            return Ok(HttpFetchResult {
                heads: remote_heads,
                tags: remote_tags,
                all_advertised,
                object_format,
            });
        }
    }

    let req = write_fetch_request(true)?;
    let mut resp = http_post(
        client,
        &post_url,
        &format!("application/x-{SERVICE}-request"),
        &format!("application/x-{SERVICE}-result"),
        &req,
    )?;

    let mut reader = resp.reader()?;
    let _ = process_v2_fetch_response(
        &mut reader,
        local_git_dir,
        filter_active,
        progress,
        &mut remote_heads,
        &mut remote_tags,
        &mut all_advertised,
    )?;

    crate::trace_packet::trace_packet_line(b"clone> packfile negotiation complete");
    Ok(HttpFetchResult {
        heads: remote_heads,
        tags: remote_tags,
        all_advertised,
        object_format,
    })
}

/// Best-effort default branch from `ls-refs` (`HEAD` symref target or first `refs/heads/*`).
pub fn remote_default_branch_from_advertised(adv: &[LsRefEntry]) -> Option<String> {
    for e in adv {
        if e.name == "HEAD" {
            // v2 ls-refs includes symref-target in line - we only store name+oid here;
            // HEAD oid often points at a branch tip; find matching branch.
            for h in adv {
                if h.name.starts_with("refs/heads/") && h.oid == e.oid {
                    return h.name.strip_prefix("refs/heads/").map(str::to_owned);
                }
            }
        }
    }
    adv.iter()
        .find(|e| e.name.starts_with("refs/heads/"))
        .and_then(|e| e.name.strip_prefix("refs/heads/"))
        .map(str::to_owned)
}

#[cfg(test)]
mod parse_v0_v1_advertisement_tests {
    use super::*;
    use grit_lib::objects::ObjectId;
    use grit_lib::pkt_line;

    #[test]
    fn drops_malicious_traversal_ref() {
        let oid = ObjectId::from_hex("aabbccddeeff00112233445566778899aabbccdd").unwrap();
        let mut body = Vec::new();
        pkt_line::write_line_to_vec(&mut body, &format!("{oid} refs/heads/../../../config"))
            .unwrap();
        body.extend_from_slice(b"0000");

        let (refs, _caps) = parse_v0_v1_advertisement(&body).unwrap();
        assert!(refs.is_empty(), "malformed ref must not be advertised");
    }

    #[test]
    fn keeps_valid_head_and_branch() {
        let oid = ObjectId::from_hex("aabbccddeeff00112233445566778899aabbccdd").unwrap();
        let mut body = Vec::new();
        let head = format!("{oid} HEAD\0multi_ack symref=HEAD:refs/heads/main object-format=sha1");
        pkt_line::write_line_to_vec(&mut body, &head).unwrap();
        pkt_line::write_line_to_vec(&mut body, &format!("{oid} refs/heads/main")).unwrap();
        body.extend_from_slice(b"0000");

        let (refs, caps) = parse_v0_v1_advertisement(&body).unwrap();
        assert!(caps.contains("object-format=sha1"));
        assert_eq!(refs.len(), 2);
        assert!(refs.iter().any(|r| r.name == "HEAD"));
        assert!(refs.iter().any(|r| r.name == "refs/heads/main"));
    }

    #[test]
    fn parse_ls_refs_v2_keeps_head_and_drops_traversal() {
        let oid = ObjectId::from_hex("aabbccddeeff00112233445566778899aabbccdd").unwrap();
        let malicious = "refs/heads/../../../config";
        let mut body = Vec::new();
        pkt_line::write_line_to_vec(
            &mut body,
            &format!("{oid} HEAD symref-target:refs/heads/main"),
        )
        .unwrap();
        pkt_line::write_line_to_vec(&mut body, &format!("{oid} {malicious}")).unwrap();
        pkt_line::write_line_to_vec(&mut body, &format!("{oid} refs/heads/main")).unwrap();
        body.extend_from_slice(b"0000");

        let entries = parse_ls_refs_v2_response(&body).unwrap();
        assert!(entries.iter().any(|e| e.name == "HEAD"));
        assert!(entries.iter().any(|e| e.name == "refs/heads/main"));
        assert!(
            !entries.iter().any(|e| e.name.contains("../")),
            "traversal ref must be dropped"
        );
    }
}

#[cfg(test)]
mod upload_pack_streaming_tests {
    use super::*;
    use crate::http_transport::{
        read_http_body, read_sideband_pack_to_writer, write_sideband_pack_to_temp,
        HttpTransportTimeouts, SidebandProgress,
    };
    use grit_lib::objects::{ObjectId, ObjectKind};
    use grit_lib::repo::{init_repository, Repository};
    use grit_lib::transfer::{build_pack, PackBuildOptions};
    use std::io::{Cursor, Write};
    use tempfile::TempDir;

    const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

    fn write_commit(repo: &Repository, parents: &[ObjectId], msg: &str) -> ObjectId {
        let mut body = format!("tree {EMPTY_TREE}\n");
        for p in parents {
            body.push_str(&format!("parent {}\n", p.to_hex()));
        }
        body.push_str("author A <a@x> 1 +0000\ncommitter A <a@x> 1 +0000\n\n");
        body.push_str(msg);
        body.push('\n');
        repo.odb
            .write_loose_materialize(ObjectKind::Commit, body.as_bytes())
            .expect("write commit")
    }

    fn append_sideband_pkt(out: &mut Vec<u8>, band: u8, data: &[u8]) {
        let len = 4 + 1 + data.len();
        write!(out, "{len:04x}").unwrap();
        out.push(band);
        out.extend_from_slice(data);
    }

    fn sideband_stream_for_pack(pack: &[u8]) -> Vec<u8> {
        let mut stream = Vec::new();
        append_sideband_pkt(&mut stream, 1, pack);
        stream.extend_from_slice(b"0000");
        stream
    }

    #[test]
    fn sideband_band_three_is_fatal() {
        let mut stream = Vec::new();
        append_sideband_pkt(&mut stream, 3, b"access denied\n");
        stream.extend_from_slice(b"0000");
        let mut sink = Vec::new();
        let err = read_sideband_pack_to_writer(
            &mut Cursor::new(stream),
            &mut sink,
            SidebandProgress::default(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("access denied"));
    }

    #[test]
    fn sideband_without_pack_fails() {
        let mut stream = Vec::new();
        append_sideband_pkt(&mut stream, 2, b"progress\n");
        stream.extend_from_slice(b"0000");
        let mut sink = Vec::new();
        let err = read_sideband_pack_to_writer(
            &mut Cursor::new(stream),
            &mut sink,
            SidebandProgress::default(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("missing packfile"));
    }

    #[test]
    fn spooled_upload_pack_sideband_stream_ingests_pack() {
        let dir = TempDir::new().unwrap();
        let git_dir = dir.path().join("bare.git");
        init_repository(&git_dir, true, "main", None, "files").unwrap();
        let repo = Repository::open(&git_dir, None).unwrap();
        let commit = write_commit(&repo, &[], "hello pack stream");
        let pack = build_pack(&repo.odb, &[commit], &[], &PackBuildOptions::default()).unwrap();
        let sideband = sideband_stream_for_pack(&pack);

        let timeouts = HttpTransportTimeouts::from_config(&grit_lib::config::ConfigSet::new());
        let mut spooled =
            read_http_body(Cursor::new(&sideband), "upload-pack", true, &timeouts).unwrap();
        let mut reader = spooled.reader().unwrap();
        let dest = TempDir::new().unwrap();
        let dest_git = dest.path().join("dest.git");
        init_repository(&dest_git, true, "main", None, "files").unwrap();
        let pack_path =
            write_sideband_pack_to_temp(&dest_git, &mut reader, SidebandProgress::default())
                .unwrap();
        crate::fetch_transport::unpack_upload_pack_file(&dest_git, &pack_path, false).unwrap();
        let fetched = Repository::open(&dest_git, None).unwrap();
        assert!(fetched.odb.read(&commit).is_ok());
    }

    #[test]
    fn process_v2_fetch_response_ingests_sideband_pack() {
        let dir = TempDir::new().unwrap();
        let git_dir = dir.path().join("bare.git");
        init_repository(&git_dir, true, "main", None, "files").unwrap();
        let repo = Repository::open(&git_dir, None).unwrap();
        let commit = write_commit(&repo, &[], "v2 fetch pack");
        let pack = build_pack(&repo.odb, &[commit], &[], &PackBuildOptions::default()).unwrap();

        let mut body = Vec::new();
        pkt_line::write_line_to_vec(&mut body, "packfile").unwrap();
        body.extend_from_slice(&sideband_stream_for_pack(&pack));
        pkt_line::write_flush(&mut body).unwrap();

        let mut heads = Vec::new();
        let mut tags = Vec::new();
        let mut adv = Vec::new();
        let got = process_v2_fetch_response(
            &mut body.as_slice(),
            &git_dir,
            false,
            SidebandProgress::default(),
            &mut heads,
            &mut tags,
            &mut adv,
        )
        .unwrap();
        assert!(got);
        let fetched = Repository::open(&git_dir, None).unwrap();
        assert!(fetched.odb.read(&commit).is_ok());
    }
}
