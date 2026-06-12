//! Wire-protocol fetch orchestration over a [`crate::transport::Connection`].
//!
//! [`fetch_remote`] is the wire counterpart to [`crate::transfer::fetch_local`]:
//! instead of copying objects between two on-disk repositories, it drives a
//! `git-upload-pack` negotiation over a live [`crate::transport::Connection`] —
//! resolving wanted oids from the connection's advertised refs (via the same
//! refspec matching `fetch_local` uses), running the
//! [`crate::fetch_negotiator::SkippingNegotiator`] `want`/`have`/`done`
//! exchange, demultiplexing the side-band pack, ingesting it with
//! [`crate::unpack_objects`], and classifying ref updates into the shared
//! [`crate::transfer::FetchOutcome`].
//!
//! This is the protocol-v0/v1 negotiation loop lifted from the CLI's
//! `fetch_transport::fetch_upload_pack_negotiate_pack_bytes_with_streams`,
//! generalized to run over the [`crate::transport::Connection`] reader/writer
//! rather than subprocess pipes.
//!
//! Protocol v2 over the streaming transports (`git://`, ssh) is also handled
//! here: a v2 [`crate::transport::Connection`] advertises no refs on connect, so
//! [`fetch_remote`] first issues a `command=ls-refs` (deriving ref-prefixes from
//! the fetch refspecs) to recover the ref map, then runs a `command=fetch`
//! negotiation — multi-round `want`/`have`/`done` with the same
//! [`crate::fetch_negotiator::SkippingNegotiator`] — and demuxes the
//! side-band-64k pack from the `packfile` section. Both paths share the refspec
//! matching, tag-mode, prune, classification, and pack-ingest plumbing. The v2
//! request fragments are lifted from the CLI's `file_upload_pack_v2` /
//! `fetch_transport` (`write_v2_fetch_request`, `read_v2_acknowledgments`,
//! `read_v2_fetch_pack_response`, `v2_ls_refs_for_fetch`). Smart-HTTP stays on
//! v0/v1 (its stateless multi-POST v2 flow is out of scope for this pass).

use std::collections::HashSet;
use std::io::{Read, Write};
use std::path::Path;

use crate::error::{Error, Result};
use crate::fetch_negotiator::SkippingNegotiator;
use crate::objects::ObjectId;
use crate::pkt_line;
use crate::protocol_v2;
use crate::refspec::{parse_fetch_refspec, RefspecItem};
use crate::transfer::{
    classify_update, match_positive, open_odb, prune_tracking_refs, ref_excluded, refspecs_force,
    FetchOptions, FetchOutcome, RefUpdate, UpdateMode,
};
use crate::transport::Connection;

/// Sink for the remote's human-readable progress (side-band channel 2).
///
/// Implementations receive the raw progress bytes the server writes (typically
/// `\r`-delimited counter lines). The default does nothing.
pub trait Progress {
    /// Receive a chunk of progress bytes from side-band channel 2.
    fn message(&mut self, _bytes: &[u8]) {}
}

/// A [`Progress`] that discards everything.
pub struct NoProgress;

impl Progress for NoProgress {}

// --- Negotiation flush schedule (mirrors fetch-pack.c) --------------------

const INITIAL_FLUSH: usize = 16;
const PIPESAFE_FLUSH: usize = 32;

fn next_flush_count(count: usize) -> usize {
    if count < PIPESAFE_FLUSH {
        count * 2
    } else {
        count + PIPESAFE_FLUSH
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum AckKind {
    /// `ACK <oid>` with no status suffix (post-`done` or legacy).
    Bare,
    Common,
    Continue,
    Ready,
}

fn parse_ack(line: &str) -> Option<(ObjectId, AckKind)> {
    if line == "NAK" {
        return None;
    }
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
    Some((oid, kind))
}

/// Read one ACK round, feeding `common`/`continue`/`ready` acks to the
/// negotiator. Lifted from `read_ack_round_with_negotiator`.
fn read_ack_round(reader: &mut dyn Read, negotiator: &mut SkippingNegotiator) -> Result<()> {
    let mut reader = reader;
    loop {
        let Some(pkt) = pkt_line::read_packet(&mut reader)? else {
            break;
        };
        match pkt {
            pkt_line::Packet::Flush => break,
            pkt_line::Packet::Data(ln) => {
                let ln = ln.trim_end();
                if ln == "NAK" {
                    // `upload-pack` sends `NAK` as the last line of a round with no trailing
                    // flush; waiting for another packet would block forever.
                    break;
                }
                let Some((ack_oid, kind)) = parse_ack(ln) else {
                    break;
                };
                if kind == AckKind::Bare {
                    break;
                }
                let _ = negotiator.ack(ack_oid)?;
            }
            _ => {}
        }
    }
    Ok(())
}

/// Read a raw pkt-line payload (length-prefixed), returning `None` on
/// flush/delim/response-end/EOF. Side-band readers stop at a flush.
fn read_pkt_payload_raw(r: &mut dyn Read) -> std::io::Result<Option<Vec<u8>>> {
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
            let payload_len = n - 4;
            let mut buf = vec![0u8; payload_len];
            r.read_exact(&mut buf)?;
            Ok(Some(buf))
        }
    }
}

/// Demultiplex the side-band-64k stream after `done`: collect channel-1 pack
/// bytes into `out` (scanning for the `PACK` magic, which may span chunk
/// boundaries), and forward channel-2 progress to `progress`. Channel 3 is a
/// fatal error. Lifted from `read_sideband_pack_until_done`.
fn read_sideband_pack(
    r: &mut dyn Read,
    out: &mut Vec<u8>,
    progress: &mut dyn Progress,
) -> Result<()> {
    let mut seen_pack = false;
    let mut pending: Vec<u8> = Vec::new();
    loop {
        let Some(payload) = read_pkt_payload_raw(r)? else {
            break;
        };
        if payload.is_empty() {
            continue;
        }
        match payload[0] {
            1 => {
                let data = &payload[1..];
                if seen_pack {
                    out.extend_from_slice(data);
                } else {
                    pending.extend_from_slice(data);
                    if let Some(pos) = pending.windows(4).position(|w| w == b"PACK") {
                        seen_pack = true;
                        out.extend_from_slice(&pending[pos..]);
                        pending.clear();
                    } else if pending.len() > 3 {
                        let keep_from = pending.len() - 3;
                        pending.drain(..keep_from);
                    }
                }
            }
            2 => progress.message(&payload[1..]),
            3 => {
                return Err(Error::Message(format!(
                    "remote error: {}",
                    String::from_utf8_lossy(&payload[1..]).trim_end()
                )));
            }
            _ => {
                // No side-band: raw pack bytes.
                if !seen_pack && payload.starts_with(b"PACK") {
                    seen_pack = true;
                    out.extend_from_slice(&payload);
                } else if seen_pack {
                    out.extend_from_slice(&payload);
                }
            }
        }
    }
    Ok(())
}

/// Peel `oid` to the commit usable as a negotiation tip; `None` if it is not a
/// commit (or is missing). Mirrors the CLI's `peel_commit_oid_for_negotiation`
/// but tolerates missing/non-commit objects by returning `None`.
fn peel_to_commit(repo: &crate::repo::Repository, oid: ObjectId) -> Option<ObjectId> {
    let mut current = oid;
    for _ in 0..16 {
        let obj = repo.odb.read(&current).ok()?;
        match obj.kind {
            crate::objects::ObjectKind::Commit => return Some(current),
            crate::objects::ObjectKind::Tag => {
                current = crate::objects::parse_tag(&obj.data).ok()?.object;
            }
            _ => return None,
        }
    }
    None
}

/// New shallow boundaries the server reported during a fetch, captured from the
/// `shallow-info` section so [`fetch_remote`] (and the HTTP fetch paths) can
/// update the local `shallow` file and surface them in [`FetchOutcome`].
#[derive(Default)]
pub(crate) struct ShallowUpdate {
    pub(crate) shallow: Vec<ObjectId>,
    pub(crate) unshallow: Vec<ObjectId>,
}

/// Append the v0/v1 shallow/deepen request lines (after the `want`s, before the
/// terminating flush): the client's current `shallow <oid>` grafts and any
/// `deepen` / `deepen-since` / `deepen-not` the caller requested. Gated on the
/// matching server capability where one exists. Mirrors the CLI's
/// `append_fetch_request_extensions_v0_v1`.
fn append_shallow_request_v0(
    req: &mut Vec<u8>,
    server_caps: &str,
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
        if server_caps.contains("deepen-since") {
            let value = crate::shallow::deepen_since_wire_value(since);
            pkt_line::write_line_to_vec(req, &format!("deepen-since {value}"))?;
        }
    }
    if server_caps.contains("deepen-not") {
        for excl in &opts.deepen_not {
            let excl = excl.trim();
            if !excl.is_empty() {
                pkt_line::write_line_to_vec(req, &format!("deepen-not {excl}"))?;
            }
        }
    }
    Ok(())
}

/// Negotiate with `git-upload-pack` over the connection and return the raw
/// packfile bytes for the requested `wants`, plus any shallow-boundary updates
/// the server reported (`shallow`/`unshallow`).
///
/// Drives the [`SkippingNegotiator`] over the connection: sends `want` lines
/// (with v0/v1 capabilities) and the advertised refs as `known_common`, batches
/// local `have`s with flushes (reading interleaved ACK rounds), sends `done`,
/// consumes the final ACK/NAK, then demuxes the side-band pack.
///
/// When `opts` requests a deepen (or the repo is already shallow), the `want`
/// block carries the client's `shallow <oid>` grafts and the `deepen*` args, and
/// the server precedes the pack with a `shallow-info` section that this reads
/// into the returned [`ShallowUpdate`].
fn negotiate_pack(
    local_git_dir: &Path,
    conn: &mut dyn Connection,
    wants: &[ObjectId],
    opts: &FetchOptions,
    local_shallow: &[ObjectId],
    progress: &mut dyn Progress,
) -> Result<(Vec<u8>, ShallowUpdate)> {
    let local_repo = crate::repo::Repository::open(local_git_dir, None)?;
    let want_set: HashSet<ObjectId> = wants.iter().copied().collect();

    let Some(first_want) = wants.first().copied() else {
        return Ok((Vec::new(), ShallowUpdate::default()));
    };

    // A deepen/shallow request changes the negotiation: the server precedes the
    // pack with a `shallow-info` section, and the client's local history is not a
    // usable negotiation base (its objects bottom out at grafts), so we skip
    // offering `have`s. Mirrors `fetch-pack.c`'s shallow handling.
    let shallow_request = opts.has_deepen_request() || !local_shallow.is_empty();

    // Capability set matching `git fetch-pack`'s first `want` line for v0/v1.
    let caps = " multi_ack_detailed side-band-64k thin-pack no-progress include-tag ofs-delta agent=grit";

    // Capture the advertised refs before borrowing the writer (avoids aliasing
    // the connection's reader/writer with its accessors). v0/v1 shallow servers
    // append `shallow <oid>` trailer lines to the advertisement; the capability
    // string we read from the advertisement drives `deepen-since`/`deepen-not`.
    let advertised: Vec<(String, ObjectId)> = conn.advertised_refs().to_vec();
    let server_caps: String = conn.capabilities().join(" ");

    let mut req: Vec<u8> = Vec::new();
    let w0 = format!("want {}{}", first_want.to_hex(), caps);
    pkt_line::write_line_to_vec(&mut req, &w0)?;
    for w in wants.iter().skip(1) {
        pkt_line::write_line_to_vec(&mut req, &format!("want {}", w.to_hex()))?;
    }
    // Match `git fetch-pack`: with a single unique OID, repeat the bare want.
    // git-daemon expects this. (Not done for shallow requests, which append
    // shallow/deepen lines instead.)
    if wants.len() == 1 && !shallow_request {
        pkt_line::write_line_to_vec(&mut req, &format!("want {}", first_want.to_hex()))?;
    }
    append_shallow_request_v0(&mut req, &server_caps, local_shallow, opts)?;
    req.extend_from_slice(b"0000");
    conn.writer().write_all(&req)?;
    conn.writer().flush()?;

    // Build the negotiator from local ref tips (heads, tags, HEAD), peeled to
    // commits, excluding the wants. Advertised tips we already have become
    // `known_common`.
    let mut negotiator = SkippingNegotiator::new(local_repo);
    let mut tips: Vec<ObjectId> = Vec::new();
    let mut seen_tip: HashSet<ObjectId> = HashSet::new();
    for prefix in ["refs/heads/", "refs/tags/"] {
        if let Ok(entries) = crate::refs::list_refs(local_git_dir, prefix) {
            for (_, oid) in entries {
                if let Some(c) = peel_to_commit(negotiator.repo(), oid) {
                    if !want_set.contains(&c) && seen_tip.insert(c) {
                        tips.push(c);
                    }
                }
            }
        }
    }
    if let Ok(h) = crate::refs::resolve_ref(local_git_dir, "HEAD") {
        if let Some(c) = peel_to_commit(negotiator.repo(), h) {
            if !want_set.contains(&c) && seen_tip.insert(c) {
                tips.push(c);
            }
        }
    }
    tips.sort_by_key(ObjectId::to_hex);
    if !shallow_request {
        for t in tips {
            negotiator.add_tip(t)?;
        }
        for (_, oid) in &advertised {
            if want_set.contains(oid) {
                continue;
            }
            if let Some(c) = peel_to_commit(negotiator.repo(), *oid) {
                negotiator.known_common(c)?;
            }
        }
    }

    // Shallow-info section: for a deepen/shallow request the v0/v1 server emits
    // its `shallow`/`unshallow` lines (flush-terminated) immediately after the
    // wants block, before any ACK round. Read it now so the subsequent ACK/NAK
    // and pack reads line up.
    let mut shallow_update = ShallowUpdate::default();
    if shallow_request {
        let (sh, unsh) = crate::shallow::read_shallow_info_section(&mut conn.reader())?;
        shallow_update.shallow = sh;
        shallow_update.unshallow = unsh;
    }

    // Have/ACK exchange: batch haves, flush, read interleaved ACK rounds.
    let mut count: usize = 0;
    let mut flush_at: usize = INITIAL_FLUSH;
    let mut pending: Vec<u8> = Vec::new();
    let mut flushes: i32 = 0;
    while let Some(oid) = negotiator.next_have()? {
        pkt_line::write_line_to_vec(&mut pending, &format!("have {}", oid.to_hex()))?;
        count += 1;
        if flush_at <= count {
            pending.extend_from_slice(b"0000");
            conn.writer().write_all(&pending)?;
            conn.writer().flush()?;
            pending.clear();
            flush_at = next_flush_count(count);
            flushes += 1;
            // Keep one window ahead: skip reading ACKs after the first flush.
            if count == INITIAL_FLUSH {
                continue;
            }
            read_ack_round(conn.reader(), &mut negotiator)?;
            flushes -= 1;
        }
    }
    if !pending.is_empty() {
        pending.extend_from_slice(b"0000");
        conn.writer().write_all(&pending)?;
        conn.writer().flush()?;
        flushes += 1;
    }
    while flushes > 0 {
        read_ack_round(conn.reader(), &mut negotiator)?;
        flushes -= 1;
    }

    // Send `done` (single pkt-line, no trailing flush) and read the ACK/NAK.
    let mut tail = Vec::new();
    pkt_line::write_line_to_vec(&mut tail, "done")?;
    conn.writer().write_all(&tail)?;
    conn.writer().flush()?;

    match pkt_line::read_packet(&mut conn.reader())? {
        None => return Err(Error::Message("unexpected EOF after done".to_owned())),
        Some(pkt_line::Packet::Flush) => {
            return Err(Error::Message("unexpected flush after done".to_owned()))
        }
        Some(pkt_line::Packet::Data(ln)) => {
            let ln = ln.trim_end();
            if ln != "NAK" {
                if let Some((ack_oid, kind)) = parse_ack(ln) {
                    if kind != AckKind::Bare {
                        let _ = negotiator.ack(ack_oid)?;
                    }
                } else if let Some(msg) = ln.strip_prefix("ERR ") {
                    return Err(Error::Message(format!("remote error: {}", msg.trim_end())));
                }
            }
        }
        Some(_) => {}
    }

    let mut pack = Vec::new();
    read_sideband_pack(conn.reader(), &mut pack, progress)?;
    Ok((pack, shallow_update))
}

// ===========================================================================
// Protocol v2 (streaming transports: git://, ssh)
// ===========================================================================
//
// A v2 connection advertises no refs on connect (only the capability block).
// `v2_ls_refs` recovers the ref map with a `command=ls-refs`; `negotiate_pack_v2`
// runs the `command=fetch` negotiation and returns the demuxed pack. Both lift
// the exact pkt-line shapes from the CLI's `file_upload_pack_v2` /
// `fetch_transport` v2 paths and reuse `protocol_v2` cap helpers, the shared
// `SkippingNegotiator`, and `read_sideband_pack`.

/// The `object-format=` value to put on the wire for a v2 request: echo the
/// server's advertised object-format when present, else fall back to the local
/// odb's hash algorithm (sha1/sha256). Keeps the negotiation hash-algo-aware.
pub(crate) fn v2_object_format(server_caps: &[String], local_odb: &crate::odb::Odb) -> String {
    for c in server_caps {
        if let Some(fmt) = c.strip_prefix("object-format=") {
            let f = fmt.trim();
            if !f.is_empty() {
                return f.to_ascii_lowercase();
            }
        }
    }
    local_odb.hash_algo().name().to_owned()
}

/// Derive `ref-prefix` lines for `command=ls-refs` from the fetch refspecs, port
/// of the CLI's `v2_ref_prefixes_from_refspecs`. A `refs/...` source maps to its
/// literal directory prefix (up to the first `*`); a bare name maps under
/// `refs/heads/`. `HEAD` is requested as a literal prefix.
fn v2_ref_prefixes_from_refspecs(refspecs: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let push_unique = |out: &mut Vec<String>, value: &str| {
        if !out.iter().any(|v| v == value) {
            out.push(value.to_owned());
        }
    };
    for spec in refspecs {
        if spec.starts_with('^') {
            continue;
        }
        let raw = spec.strip_prefix('+').unwrap_or(spec.as_str());
        let src = raw.split_once(':').map(|(s, _)| s).unwrap_or(raw).trim();
        if src.is_empty() {
            continue;
        }
        if src == "HEAD" {
            push_unique(&mut out, "HEAD");
            continue;
        }
        if let Some(star) = src.find('*') {
            let prefix = &src[..star];
            if prefix.is_empty() {
                continue;
            }
            if prefix.starts_with("refs/") {
                push_unique(&mut out, prefix);
            } else {
                push_unique(&mut out, &format!("refs/heads/{prefix}"));
            }
            continue;
        }
        if src.starts_with("refs/") {
            push_unique(&mut out, src);
        } else {
            push_unique(&mut out, &format!("refs/heads/{src}"));
        }
    }
    out
}

/// Parse one v2 `ls-refs` advertisement line into `(refname, oid, symref_target)`.
///
/// Lines look like `<oid> <refname>[ symref-target:<t>][ peeled:<oid>]`. Lib-side
/// port of the CLI's `parse_ls_refs_v2_line` (the order of the optional suffixes
/// is whichever the server emits; we scan for both tokens). Returns `None` for a
/// malformed line.
fn parse_ls_refs_v2_line(line: &str) -> Option<(String, ObjectId, Option<String>)> {
    const SYM: &str = " symref-target:";
    const PEEL: &str = " peeled:";
    let (oid_hex, after_oid) = line.split_once(' ')?;
    let oid = ObjectId::from_hex(oid_hex).ok()?;

    // The refname ends at the first ` symref-target:` or ` peeled:` token.
    let sym_at = after_oid.find(SYM);
    let peel_at = after_oid.find(PEEL);
    let name_end = match (sym_at, peel_at) {
        (Some(a), Some(b)) => a.min(b),
        (Some(a), None) => a,
        (None, Some(b)) => b,
        (None, None) => after_oid.len(),
    };
    let name = after_oid[..name_end].trim().to_owned();
    if name.is_empty() {
        return None;
    }
    let symref_target = sym_at.map(|pos| {
        let tail = &after_oid[pos + SYM.len()..];
        let end = tail.find(' ').unwrap_or(tail.len());
        tail[..end].to_owned()
    });
    Some((name, oid, symref_target))
}

/// Issue `command=ls-refs` over a v2 connection and parse the ref map.
///
/// Sends the capability echo (agent/object-format via
/// [`protocol_v2::cap_lines_for_command_request`]), the `0001` delimiter, then
/// `symrefs`, `peel`, and `ref-prefix <p>` lines derived from `refspecs` (plus
/// `refs/tags/` when `tags != None`), then flush. Returns the advertised
/// `refs/heads/*` and `refs/tags/*` refs (peeled `^{}` carrier lines dropped) and
/// the `HEAD` symref target. Lifted from the CLI's `v2_ls_refs_for_fetch`.
fn v2_ls_refs(
    conn: &mut dyn Connection,
    server_caps: &[String],
    local_odb: &crate::odb::Odb,
    tags: crate::transfer::TagMode,
    refspecs: &[String],
) -> Result<(Vec<(String, ObjectId)>, Option<String>)> {
    let req = build_v2_ls_refs_request(server_caps, local_odb, tags, refspecs)?;
    conn.writer().write_all(&req)?;
    conn.writer().flush()?;
    parse_v2_ls_refs_response(conn.reader())
}

/// Build the `command=ls-refs` request body (capability echo + `0001` + the
/// `symrefs`/`peel`/`ref-prefix` argument lines + flush) for a v2 fetch.
///
/// Factored out of [`v2_ls_refs`] so the streaming transports (which write it to
/// a duplex socket) and the stateless smart-HTTP transport (which POSTs it as a
/// request body) share one request builder. `HEAD` is always requested so the
/// server advertises its `symref-target`; `refs/tags/` is added under `--tags` /
/// tag-following even when the refspecs name only heads.
pub(crate) fn build_v2_ls_refs_request(
    server_caps: &[String],
    local_odb: &crate::odb::Odb,
    tags: crate::transfer::TagMode,
    refspecs: &[String],
) -> Result<Vec<u8>> {
    let object_format = v2_object_format(server_caps, local_odb);
    let cap_echo = protocol_v2::cap_lines_for_command_request(server_caps);

    let mut req: Vec<u8> = Vec::new();
    pkt_line::write_line(&mut req, "command=ls-refs")?;
    // Echo agent/object-format; if the server advertised neither (rare), still
    // pin the object-format so a sha256 server agrees on hash width.
    if cap_echo.iter().any(|c| c.starts_with("object-format=")) {
        for line in &cap_echo {
            pkt_line::write_line(&mut req, line)?;
        }
    } else {
        for line in &cap_echo {
            pkt_line::write_line(&mut req, line)?;
        }
        pkt_line::write_line(&mut req, &format!("object-format={object_format}"))?;
    }
    pkt_line::write_delim(&mut req)?;
    pkt_line::write_line(&mut req, "symrefs")?;
    pkt_line::write_line(&mut req, "peel")?;

    // Always request `HEAD` so the server advertises its `symref-target`, which
    // drives `FetchOutcome::default_branch` (the wire equivalent of the v0/v1
    // `symref=HEAD:` capability). `HEAD` is dropped from the fetchable ref set.
    pkt_line::write_line(&mut req, "ref-prefix HEAD")?;
    let mut prefixes = v2_ref_prefixes_from_refspecs(refspecs);
    if prefixes.is_empty() {
        prefixes.push("refs/heads/".to_owned());
        prefixes.push("refs/tags/".to_owned());
    } else if tags != crate::transfer::TagMode::None
        && !prefixes.iter().any(|p| p == "refs/tags/")
    {
        // Tag-following / `--tags` wants the tag namespace advertised so we can
        // add tags from the ls-refs result, even if the refspecs only name heads.
        prefixes.push("refs/tags/".to_owned());
    }
    for p in &prefixes {
        pkt_line::write_line(&mut req, &format!("ref-prefix {p}"))?;
    }
    pkt_line::write_flush(&mut req)?;
    Ok(req)
}

/// Parse a `command=ls-refs` response into `(advertised refs, HEAD symref)`.
///
/// Reads `<oid> <refname>[ symref-target:…][ peeled:…]` lines up to the
/// terminating flush, dropping peeled `^{}` carriers and recording the `HEAD`
/// symref target. Shared by the streaming and stateless-HTTP v2 paths.
pub(crate) fn parse_v2_ls_refs_response(
    reader: &mut dyn Read,
) -> Result<(Vec<(String, ObjectId)>, Option<String>)> {
    // Response: `<oid> <refname>[ symref-target:…][ peeled:…]` lines, flush-terminated.
    let mut advertised: Vec<(String, ObjectId)> = Vec::new();
    let mut head_symref: Option<String> = None;
    let mut reader = reader;
    loop {
        match pkt_line::read_packet(&mut reader)? {
            None | Some(pkt_line::Packet::Flush) | Some(pkt_line::Packet::Delim) => break,
            Some(pkt_line::Packet::ResponseEnd) => break,
            Some(pkt_line::Packet::Data(line)) => {
                let line = line.trim_end_matches('\n');
                if let Some(msg) = line.strip_prefix("ERR ") {
                    return Err(Error::Message(format!(
                        "remote error: {}",
                        msg.trim_end()
                    )));
                }
                let Some((name, oid, symref_target)) = parse_ls_refs_v2_line(line) else {
                    continue;
                };
                if name.contains("^{") || name.ends_with("^{}") {
                    continue;
                }
                if name == "HEAD" {
                    if let Some(t) = symref_target {
                        head_symref = Some(t);
                    }
                    // HEAD itself is not a fetchable ref here; refspecs target heads/tags.
                    continue;
                }
                if name.starts_with("refs/heads/")
                    || name.starts_with("refs/tags/")
                    || name.starts_with("refs/")
                {
                    advertised.push((name, oid));
                }
            }
        }
    }
    Ok((advertised, head_symref))
}

/// Build the ordered `have` candidate list for a v2 fetch from the local ref
/// tips (heads, tags, HEAD), peeled to commits and excluding the wants, driven
/// through the [`SkippingNegotiator`]'s skipping schedule.
///
/// Shared by the streaming (`negotiate_pack_v2`) and stateless-HTTP v2 fetch
/// paths so both offer the server the same `have`s in the same order. The wire
/// rounds (how many haves per request, when to send `done`) are batched by the
/// caller, which differs between a duplex socket and stateless POSTs.
pub(crate) fn v2_local_haves(
    local_git_dir: &Path,
    wants: &[ObjectId],
) -> Result<Vec<ObjectId>> {
    let want_set: HashSet<ObjectId> = wants.iter().copied().collect();
    let local_repo = crate::repo::Repository::open(local_git_dir, None)?;
    let mut negotiator = SkippingNegotiator::new(local_repo);
    let mut tips: Vec<ObjectId> = Vec::new();
    let mut seen_tip: HashSet<ObjectId> = HashSet::new();
    for prefix in ["refs/heads/", "refs/tags/"] {
        if let Ok(entries) = crate::refs::list_refs(local_git_dir, prefix) {
            for (_, oid) in entries {
                if let Some(c) = peel_to_commit(negotiator.repo(), oid) {
                    if !want_set.contains(&c) && seen_tip.insert(c) {
                        tips.push(c);
                    }
                }
            }
        }
    }
    if let Ok(h) = crate::refs::resolve_ref(local_git_dir, "HEAD") {
        if let Some(c) = peel_to_commit(negotiator.repo(), h) {
            if !want_set.contains(&c) && seen_tip.insert(c) {
                tips.push(c);
            }
        }
    }
    tips.sort_by_key(ObjectId::to_hex);
    for t in tips {
        negotiator.add_tip(t)?;
    }
    // Drain the negotiator into an ordered have list (it already applies the
    // skipping schedule); the caller batches the wire rounds.
    let mut haves: Vec<ObjectId> = Vec::new();
    while let Some(oid) = negotiator.next_have()? {
        haves.push(oid);
    }
    Ok(haves)
}

/// Run a v2 `command=fetch` negotiation over the connection and return the raw
/// pack bytes for `wants`.
///
/// Drives the [`SkippingNegotiator`] exactly like the v0/v1 path, but frames the
/// request as v2 (`command=fetch`, capability echo, `0001`, then
/// `thin-pack`/`no-progress`/`ofs-delta`, `want <oid>` lines, `have <oid>` lines,
/// and `done`). Multi-round: round 1 sends the first batch of haves *without*
/// `done`, reads the `acknowledgments` section (looking for `ready`); if not yet
/// ready it sends the remaining haves + `done`. Then reads the response sections
/// (`acknowledgments`, optional `shallow-info`/`wanted-refs`, then `packfile`) and
/// demuxes the side-band-64k pack. Lifted from `write_v2_fetch_request` +
/// `read_v2_acknowledgments` / `read_v2_fetch_pack_response`.
fn negotiate_pack_v2(
    local_git_dir: &Path,
    conn: &mut dyn Connection,
    server_caps: &[String],
    local_odb: &crate::odb::Odb,
    wants: &[ObjectId],
    deepen: &V2DeepenArgs,
    progress: &mut dyn Progress,
) -> Result<(Vec<u8>, ShallowUpdate)> {
    if wants.is_empty() {
        return Ok((Vec::new(), ShallowUpdate::default()));
    }
    let object_format = v2_object_format(server_caps, local_odb);
    let cap_echo = protocol_v2::cap_lines_for_command_request(server_caps);
    let sideband_all = protocol_v2::fetch_supports_sideband_all(server_caps);

    // A deepen/shallow request does not offer local haves (the local objects
    // bottom out at grafts and are not a usable negotiation base), forcing the
    // single-round path so the server sends a `shallow-info` section + pack.
    let shallow_request = deepen.is_shallow_request();

    // The ordered `have` list, built from the local ref tips with the skipping
    // negotiator (shared with the stateless-HTTP v2 path). Empty for a shallow
    // request.
    let haves = if shallow_request {
        Vec::new()
    } else {
        v2_local_haves(local_git_dir, wants)?
    };

    let mut pack = Vec::new();
    let mut shallow_update = ShallowUpdate::default();
    if haves.is_empty() {
        // No local history to offer: single round, wants + done, read the pack.
        write_v2_fetch_request(
            conn.writer(),
            &object_format,
            &cap_echo,
            wants,
            &[],
            sideband_all,
            deepen,
            true,
        )?;
        read_v2_fetch_pack_response(conn.reader(), &mut pack, &mut shallow_update, progress)?;
        return Ok((pack, shallow_update));
    }

    // Multi-round: round 1 sends the first batch of haves WITHOUT done.
    let first_batch = haves.len().min(INITIAL_FLUSH);
    write_v2_fetch_request(
        conn.writer(),
        &object_format,
        &cap_echo,
        wants,
        &haves[..first_batch],
        sideband_all,
        deepen,
        false,
    )?;

    let ack = read_v2_acknowledgments(conn.reader())?;
    match ack {
        // Server is `ready`: the pack follows in the SAME response after a delim.
        Some(round) if round.ready => {
            read_v2_fetch_pack_response(conn.reader(), &mut pack, &mut shallow_update, progress)?;
        }
        // Server skipped acknowledgments and went straight to the pack header
        // (consumed inside the reader); read the pack now.
        None => {
            read_v2_fetch_pack_response(conn.reader(), &mut pack, &mut shallow_update, progress)?;
        }
        // Not ready yet: round 2 sends the remaining haves + `done`, then pack.
        Some(_) => {
            write_v2_fetch_request(
                conn.writer(),
                &object_format,
                &cap_echo,
                wants,
                &haves[first_batch..],
                sideband_all,
                deepen,
                true,
            )?;
            read_v2_fetch_pack_response(conn.reader(), &mut pack, &mut shallow_update, progress)?;
        }
    }
    Ok((pack, shallow_update))
}

/// The shallow/deepen arguments for a v2 `command=fetch` request, derived from
/// [`FetchOptions`] plus the local `shallow` file. Built once by the fetch driver
/// and passed to [`write_v2_fetch_request`] on each round (every stateless POST
/// must resend them).
#[derive(Clone, Default)]
pub(crate) struct V2DeepenArgs {
    /// The client's current shallow grafts (`shallow <oid>` lines).
    pub(crate) local_shallow: Vec<ObjectId>,
    /// `deepen <n>` (absolute depth, or `INFINITE_DEPTH` for `--unshallow`).
    pub(crate) depth: Option<u32>,
    /// `deepen-since <unix-ts>`.
    pub(crate) deepen_since: Option<String>,
    /// `deepen-not <ref>` exclusions.
    pub(crate) deepen_not: Vec<String>,
}

impl V2DeepenArgs {
    /// Build the v2 deepen args from the fetch options and the local shallow file,
    /// translating `--unshallow` into the `INFINITE_DEPTH` deepen Git uses.
    pub(crate) fn from_opts(opts: &FetchOptions, local_shallow: &[ObjectId]) -> Self {
        let depth = if opts.unshallow {
            Some(crate::shallow::INFINITE_DEPTH)
        } else {
            opts.depth.filter(|d| *d > 0)
        };
        Self {
            local_shallow: local_shallow.to_vec(),
            depth,
            deepen_since: opts
                .deepen_since
                .as_deref()
                .filter(|s| !s.trim().is_empty())
                .map(crate::shallow::deepen_since_wire_value),
            deepen_not: opts
                .deepen_not
                .iter()
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty())
                .collect(),
        }
    }

    /// Whether any deepen/shallow argument is present (drives `shallow-info`
    /// handling and the "skip offering haves" decision).
    pub(crate) fn is_shallow_request(&self) -> bool {
        self.depth.is_some()
            || self.deepen_since.is_some()
            || !self.deepen_not.is_empty()
            || !self.local_shallow.is_empty()
    }
}

/// Write a v2 `command=fetch` request: capability echo, `0001`, the standard
/// `thin-pack`/`no-progress`/`ofs-delta` (+ `sideband-all`/`include-tag`)
/// arguments, the shallow/deepen arguments, the `want <oid>` lines, the
/// `have <oid>` lines, and `done` when `send_done`, terminated by flush. Lifted
/// from the CLI's `write_v2_fetch_request` (streaming-fetch subset).
pub(crate) fn write_v2_fetch_request(
    w: &mut dyn Write,
    object_format: &str,
    cap_echo: &[String],
    wants: &[ObjectId],
    haves: &[ObjectId],
    sideband_all: bool,
    deepen: &V2DeepenArgs,
    send_done: bool,
) -> Result<()> {
    let mut req: Vec<u8> = Vec::new();
    pkt_line::write_line(&mut req, "command=fetch")?;
    if cap_echo.iter().any(|c| c.starts_with("object-format=")) {
        for line in cap_echo {
            pkt_line::write_line(&mut req, line)?;
        }
    } else {
        for line in cap_echo {
            pkt_line::write_line(&mut req, line)?;
        }
        pkt_line::write_line(&mut req, &format!("object-format={object_format}"))?;
    }
    pkt_line::write_delim(&mut req)?;

    pkt_line::write_line(&mut req, "thin-pack")?;
    pkt_line::write_line(&mut req, "no-progress")?;
    pkt_line::write_line(&mut req, "ofs-delta")?;
    if sideband_all {
        pkt_line::write_line(&mut req, "sideband-all")?;
    }
    // Ask the server to bundle tag objects pointing at fetched history; the
    // TagMode plumbing in `fetch_remote` decides which tag refs to write.
    pkt_line::write_line(&mut req, "include-tag")?;

    // Shallow/deepen arguments (the `fetch` v2 command's `shallow`/`deepen*` args).
    for oid in &deepen.local_shallow {
        pkt_line::write_line(&mut req, &format!("shallow {}", oid.to_hex()))?;
    }
    if let Some(depth) = deepen.depth {
        pkt_line::write_line(&mut req, &format!("deepen {depth}"))?;
    }
    if let Some(since) = &deepen.deepen_since {
        pkt_line::write_line(&mut req, &format!("deepen-since {since}"))?;
    }
    for excl in &deepen.deepen_not {
        pkt_line::write_line(&mut req, &format!("deepen-not {excl}"))?;
    }

    for want in wants {
        pkt_line::write_line(&mut req, &format!("want {}", want.to_hex()))?;
    }
    for have in haves {
        pkt_line::write_line(&mut req, &format!("have {}", have.to_hex()))?;
    }
    if send_done {
        pkt_line::write_line(&mut req, "done")?;
    }
    pkt_line::write_flush(&mut req)?;
    w.write_all(&req)?;
    w.flush()?;
    Ok(())
}

/// Outcome of reading one v2 `acknowledgments` section.
pub(crate) struct V2AckRound {
    /// Server emitted `ready`: the packfile follows in the same response after a
    /// delimiter — the caller reads the pack now without sending more.
    pub(crate) ready: bool,
}

/// Read a v2 `acknowledgments` section header and its `ACK`/`NAK`/`ready` lines.
///
/// Returns `Some(round)` for an `acknowledgments` section (with `ready` set when
/// the server is ready to send the pack), or `None` when the server skipped the
/// section and started a different one (e.g. went straight to `packfile`) — in
/// which case the header has been consumed and the caller proceeds to read the
/// pack response directly. Lifted from the CLI's `read_v2_acknowledgments`.
pub(crate) fn read_v2_acknowledgments(reader: &mut dyn Read) -> Result<Option<V2AckRound>> {
    let mut reader = reader;
    let hdr = match pkt_line::read_packet(&mut reader)? {
        Some(pkt_line::Packet::Data(s)) => s,
        Some(pkt_line::Packet::Flush) => return Ok(Some(V2AckRound { ready: false })),
        None => return Ok(None),
        Some(other) => {
            return Err(Error::Message(format!(
                "unexpected v2 fetch response: {other:?}"
            )))
        }
    };
    let hdr = hdr.trim_end();
    if let Some(msg) = hdr.strip_prefix("ERR ") {
        return Err(Error::Message(format!("remote error: {}", msg.trim_end())));
    }
    if hdr != "acknowledgments" {
        // The server started a non-acknowledgments section; the pack reader,
        // called next, re-dispatches on this header. We cannot push it back, so
        // signal `None` only when we know the pack reader will see the same
        // header — which it will, because the next read picks up where we left
        // off. To make that work, the caller treats `None` as "read the pack".
        // The header we just consumed (`shallow-info`/`wanted-refs`/`packfile`)
        // would be lost; for the streaming fetch we only reach here after a
        // first round of haves, where servers always emit `acknowledgments`
        // first. Reaching a different header is therefore unexpected.
        return Err(Error::Message(format!(
            "unexpected v2 fetch section before acknowledgments: {hdr}"
        )));
    }
    let mut ready = false;
    loop {
        match pkt_line::read_packet(&mut reader)? {
            Some(pkt_line::Packet::Data(ln)) => {
                let ln = ln.trim_end();
                if ln == "NAK" || ln.starts_with("ACK ") {
                    continue;
                }
                if ln == "ready" {
                    ready = true;
                    continue;
                }
                return Err(Error::Message(format!(
                    "unexpected acknowledgment line: '{ln}'"
                )));
            }
            Some(pkt_line::Packet::Delim) | Some(pkt_line::Packet::Flush) | None => break,
            Some(other) => {
                return Err(Error::Message(format!(
                    "unexpected acknowledgments packet: {other:?}"
                )))
            }
        }
    }
    Ok(Some(V2AckRound { ready }))
}

/// Read a v2 `command=fetch` response: capture the `shallow-info` section's
/// `shallow`/`unshallow` lines into `shallow_out`, skip the other non-pack
/// sections (`acknowledgments`/`wanted-refs`/`packfile-uris`), and demux the
/// side-band-64k pack from the `packfile` section into `out`. Lifted from the
/// CLI's `read_v2_fetch_pack_response`, extended to surface shallow updates.
pub(crate) fn read_v2_fetch_pack_response(
    reader: &mut dyn Read,
    out: &mut Vec<u8>,
    shallow_out: &mut ShallowUpdate,
    progress: &mut dyn Progress,
) -> Result<()> {
    loop {
        let hdr = match pkt_line::read_packet(&mut &mut *reader)? {
            Some(pkt_line::Packet::Data(s)) => s,
            Some(pkt_line::Packet::Flush) | None => return Ok(()),
            Some(pkt_line::Packet::Delim) => continue,
            Some(other) => {
                return Err(Error::Message(format!(
                    "unexpected v2 fetch response: {other:?}"
                )))
            }
        };
        let hdr = hdr.trim_end();
        if let Some(msg) = hdr.strip_prefix("ERR ") {
            return Err(Error::Message(format!("remote error: {}", msg.trim_end())));
        }
        match hdr {
            "shallow-info" => {
                // Capture the shallow/unshallow boundary updates. The section is
                // delim-terminated (before the `packfile` header), which
                // `read_shallow_info_section` stops at, leaving the header intact.
                let (sh, unsh) = crate::shallow::read_shallow_info_section(&mut *reader)?;
                shallow_out.shallow.extend(sh);
                shallow_out.unshallow.extend(unsh);
            }
            "acknowledgments" | "wanted-refs" | "packfile-uris" => {
                skip_v2_section_until_boundary(&mut *reader)?;
            }
            "packfile" => {
                // The `packfile` section body is side-band-64k framed; reuse the
                // shared demuxer (channel 1 = pack, channel 2 = progress, 3 = err).
                read_sideband_pack(&mut *reader, out, progress)?;
                return Ok(());
            }
            other => {
                return Err(Error::Message(format!(
                    "unexpected v2 fetch section: {other}"
                )))
            }
        }
    }
}

/// Skip a v2 response section up to its terminating flush/delim.
fn skip_v2_section_until_boundary(reader: &mut dyn Read) -> Result<()> {
    loop {
        match pkt_line::read_packet(&mut &mut *reader)? {
            None | Some(pkt_line::Packet::Flush) | Some(pkt_line::Packet::Delim) => return Ok(()),
            Some(pkt_line::Packet::ResponseEnd) => return Ok(()),
            Some(pkt_line::Packet::Data(_)) => {}
        }
    }
}

/// Fetch from a remote over a live [`Connection`], driving the upload-pack
/// negotiation and writing the resulting tracking-ref updates into
/// `local_git_dir`.
///
/// The flow mirrors [`crate::transfer::fetch_local`], but the remote ref list
/// comes from the connection's advertisement, the objects arrive over the wire
/// (negotiated pack -> [`crate::unpack_objects`]), and the local repo is opened
/// to classify ancestry. Reuses the refspec matching, tag-mode, prune, and
/// classification helpers from [`crate::transfer`].
///
/// Handles protocol v0, v1, and v2. For a v2 connection the ref map is recovered
/// via a `command=ls-refs` round (no refs are advertised on connect) and the
/// pack is negotiated with `command=fetch`; v0/v1 use the connect-time
/// advertisement and the classic `want`/`have`/`done` exchange.
///
/// # Errors
///
/// Returns an error if a refspec is invalid, if the negotiation or pack ingest
/// fails, or on ref/odb I/O failure.
pub fn fetch_remote(
    local_git_dir: &Path,
    conn: &mut dyn Connection,
    opts: &FetchOptions,
    progress: &mut dyn Progress,
) -> Result<FetchOutcome> {
    use crate::net_trace::net_trace;
    net_trace!(
        "fetch_remote: begin — protocol v{}, {} refspec(s), tags={:?}, depth={:?}",
        conn.protocol_version(),
        opts.refspecs.len(),
        opts.tags,
        opts.depth
    );
    let local_odb = open_odb(local_git_dir);

    // 1. Remote refs + default branch.
    //
    // For protocol v2 the connect-time advertisement carries no refs (only the
    // capability block); we obtain them now with an `ls-refs` command, derived
    // from the fetch refspecs. For v0/v1 they come from the connect-time
    // advertisement directly.
    let (remote_refs, default_branch, v2_caps): (
        Vec<(String, ObjectId)>,
        Option<String>,
        Option<Vec<String>>,
    ) = if conn.protocol_version() >= 2 {
        let caps: Vec<String> = conn.capabilities().to_vec();
        let (refs, head_symref) =
            v2_ls_refs(conn, &caps, &local_odb, opts.tags, &opts.refspecs)?;
        let default_branch = head_symref.map(|t| {
            t.strip_prefix("refs/heads/")
                .unwrap_or(&t)
                .to_owned()
        });
        (refs, default_branch, Some(caps))
    } else {
        let default_branch = conn.head_symref().map(|t| {
            t.strip_prefix("refs/heads/")
                .unwrap_or(t)
                .to_owned()
        });
        let remote_refs: Vec<(String, ObjectId)> = conn
            .advertised_refs()
            .iter()
            .filter(|(n, _)| n != "HEAD" && !n.ends_with("^{}"))
            .cloned()
            .collect();
        (remote_refs, default_branch, None)
    };
    net_trace!(
        "fetch_remote: remote advertised {} ref(s){}",
        remote_refs.len(),
        v2_caps
            .as_ref()
            .map(|_| " (via v2 ls-refs)")
            .unwrap_or(" (v0/v1 advertisement)")
    );

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

    // 3. Match refs to refspecs (mirror transfer::fetch_local).
    let mut matched: Vec<crate::transfer::MatchedRef> = Vec::new();
    let mut matched_oids: HashSet<ObjectId> = HashSet::new();
    let mut seen_remote_ref: HashSet<String> = HashSet::new();
    for (name, oid) in &remote_refs {
        if ref_excluded(name, &negatives) {
            continue;
        }
        if let Some(local_ref) = match_positive(name, &positive) {
            if seen_remote_ref.insert(name.clone()) {
                matched_oids.insert(*oid);
                matched.push(crate::transfer::MatchedRef {
                    remote_ref: name.clone(),
                    local_ref,
                    oid: *oid,
                    force: refspecs_force(name, &positive),
                    is_tag: name.starts_with("refs/tags/"),
                });
            }
        }
    }

    // TagMode: add tags. Tag-following needs the closure of fetched objects,
    // which we cannot compute remotely; the wire `include-tag` capability makes
    // the server send tag objects with the pack, so we add advertised tags by
    // mode here and let classification proceed once the pack lands. For
    // `Following` we approximate using the advertised remote odb if present
    // (it is not, over the wire), so we add following tags whose oid is among
    // the matched set after the fact — handled below using the local odb.
    //
    // `following_only` collects the oids of tags added *provisionally* under
    // `Following`. These must NOT be `want`ed up front: git's tag-following only
    // keeps a tag whose target is already reachable from the fetched heads, so
    // wanting the tag itself would drag down its (otherwise unreachable) target
    // and incorrectly keep the tag. They are pruned by `retain_following_tags`.
    let following_only = add_wire_tags(
        opts.tags,
        &remote_refs,
        &negatives,
        &mut matched,
        &mut matched_oids,
        &mut seen_remote_ref,
    );

    // The client's current shallow grafts (drives the wire `shallow <oid>` lines
    // and the "this is a shallow request" decisions in the negotiators).
    let local_shallow = crate::shallow::load_shallow_oids(local_git_dir)?;
    let shallow_request = opts.has_deepen_request() || !local_shallow.is_empty();

    // 4. Wants. Normally the matched oids that are absent locally. For a
    // deepen/`--unshallow` request the wanted tips may already be present (a prior
    // shallow fetch landed them); we must still `want` them so the server fills in
    // the now-reachable ancestors past the old boundary.
    let wants: Vec<ObjectId> = if shallow_request {
        matched_oids
            .iter()
            .copied()
            .filter(|oid| !following_only.contains(oid))
            .collect()
    } else {
        matched_oids
            .iter()
            .copied()
            .filter(|oid| !following_only.contains(oid) && !local_odb.exists(oid))
            .collect()
    };

    // Shallow-boundary updates the server reports (`shallow`/`unshallow`), applied
    // to the local `shallow` file and surfaced in the outcome.
    let mut shallow_update = ShallowUpdate::default();

    net_trace!(
        "fetch_remote: {} matched ref(s), want {} object(s){}",
        matched.len(),
        wants.len(),
        if shallow_request { " (shallow request)" } else { "" }
    );

    if !wants.is_empty() && !opts.dry_run {
        net_trace!("fetch_remote: negotiating + fetching pack…");
        let (pack, su) = if let Some(caps) = v2_caps.as_ref() {
            let deepen = V2DeepenArgs::from_opts(opts, &local_shallow);
            negotiate_pack_v2(local_git_dir, conn, caps, &local_odb, &wants, &deepen, progress)?
        } else {
            negotiate_pack(local_git_dir, conn, &wants, opts, &local_shallow, progress)?
        };
        shallow_update = su;
        net_trace!(
            "fetch_remote: received pack ({} bytes), unpacking…",
            pack.len()
        );
        if !pack.is_empty() {
            let mut cursor = std::io::Cursor::new(pack);
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

    // Apply the shallow/unshallow boundary updates to the on-disk `shallow` file
    // before classifying refs (so connectivity reflects the new graft set).
    if !opts.dry_run {
        crate::shallow::apply_shallow_updates(
            local_git_dir,
            &shallow_update.shallow,
            &shallow_update.unshallow,
        )?;
    }

    // Close the write side once the v2 conversation is done so the server's
    // persistent `serve_loop` sees EOF and exits — even when we sent only an
    // `ls-refs` (no wants) and skipped `command=fetch`. Without this a streaming
    // transport (ssh subprocess, daemon socket) hangs at teardown. No-op for
    // v0/v1, where the server closes after its single response.
    if v2_caps.is_some() {
        conn.finish_send();
    }

    // For TagMode::Following, prune tags whose target did not arrive in the
    // pack (now resolvable against the local odb, which holds the fetched
    // objects). All/None already handled; Following kept only when reachable.
    if opts.tags == crate::transfer::TagMode::Following {
        retain_following_tags(&local_odb, &mut matched, &matched_oids);
    }

    // 5. Classify + apply ref updates (ancestry via the now-populated local repo).
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

    net_trace!(
        "fetch_remote: done — {} ref update(s){}",
        updates.len(),
        default_branch
            .as_deref()
            .map(|b| format!(", default branch '{b}'"))
            .unwrap_or_default()
    );
    Ok(FetchOutcome {
        updates,
        default_branch,
        new_shallow: shallow_update.shallow,
        new_unshallow: shallow_update.unshallow,
    })
}

/// Add advertised tags to the matched set per [`crate::transfer::TagMode`].
///
/// Over the wire we cannot peel remote tags before the pack arrives, so:
/// * `All` adds every advertised tag (and `want`s it unconditionally).
/// * `Following` provisionally adds every advertised tag here; unreachable ones
///   are dropped by [`retain_following_tags`] after the pack is ingested.
/// * `None` adds nothing.
///
/// Returns the oids of tags added under `Following` — the caller must keep these
/// out of the `want` list so an unreachable tag does not drag its target into
/// the pack (which would make it look reachable and survive the prune).
fn add_wire_tags(
    mode: crate::transfer::TagMode,
    remote_refs: &[(String, ObjectId)],
    negatives: &[RefspecItem],
    matched: &mut Vec<crate::transfer::MatchedRef>,
    matched_oids: &mut HashSet<ObjectId>,
    seen_remote_ref: &mut HashSet<String>,
) -> HashSet<ObjectId> {
    let mut following_only: HashSet<ObjectId> = HashSet::new();
    if mode == crate::transfer::TagMode::None {
        return following_only;
    }
    for (name, oid) in remote_refs {
        if !name.starts_with("refs/tags/") {
            continue;
        }
        if seen_remote_ref.contains(name) || ref_excluded(name, negatives) {
            continue;
        }
        seen_remote_ref.insert(name.clone());
        matched_oids.insert(*oid);
        if mode == crate::transfer::TagMode::Following {
            following_only.insert(*oid);
        }
        matched.push(crate::transfer::MatchedRef {
            remote_ref: name.clone(),
            local_ref: Some(name.clone()),
            oid: *oid,
            force: false,
            is_tag: true,
        });
    }
    following_only
}

/// Drop provisional `Following` tags whose object (or peeled target) did not
/// arrive in the fetched pack — i.e. is not reachable from the other matched,
/// non-tag refs we fetched. Matches `git fetch`'s default tag-following: a tag
/// is kept when it points into the fetched history.
fn retain_following_tags(
    local_odb: &crate::odb::Odb,
    matched: &mut Vec<crate::transfer::MatchedRef>,
    matched_oids: &HashSet<ObjectId>,
) {
    // Roots: every non-tag matched ref we fetched.
    let roots: Vec<ObjectId> = matched
        .iter()
        .filter(|m| !m.is_tag)
        .map(|m| m.oid)
        .collect();
    let closure = reachable_closure(local_odb, &roots);
    matched.retain(|m| {
        if !m.is_tag {
            return true;
        }
        let peeled = peel_tag_target(local_odb, m.oid);
        // Keep when the tag object itself or its peeled target is reachable from
        // the fetched heads, and we actually have the object locally.
        let have = local_odb.exists(&m.oid);
        have && (closure.contains(&m.oid)
            || closure.contains(&peeled)
            || matched_oids.contains(&peeled))
    });
}

/// Peel an (annotated) tag to its ultimate non-tag target using the local odb.
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

/// Compute the object closure reachable from `roots` (commits -> trees ->
/// blobs, peeling tags), using the local odb. Best-effort: descent stops at
/// missing objects.
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
