//! Wire-protocol fetch orchestration over a live transport connection.
//!
//! [`fetch_remote`] drives `git-upload-pack` negotiation: match refspecs against
//! advertised refs, run `want`/`have`/`done`, demultiplex the side-band pack,
//! ingest it, and classify ref updates into a shared fetch outcome. Protocol v2
//! over streaming transports runs `ls-refs` before `fetch` when the connection
//! advertises no refs on connect.

use std::collections::HashSet;
use std::io::{Read, Write};
use std::path::Path;

use crate::environment::RepositoryOptions;
use crate::error::{Error, Result};
use crate::fetch_negotiator::SkippingNegotiator;
use crate::objects::ObjectId;
use crate::pkt_line;
use crate::protocol_v2;
use crate::refspec::RefspecItem;
use crate::repo::Repository;

type LsRefsAdvertised = (
    Vec<(String, ObjectId)>,
    Option<String>,
    std::collections::HashMap<String, ObjectId>,
    Option<ObjectId>,
);
use crate::progress::{poll_cancel, Cancel, ProgressSink};
use crate::transfer::{
    classify_update, prune_tracking_refs, ref_excluded, validate_fetch_depth_local,
    validate_fetch_depth_server, FetchOptions, FetchOutcome, RefUpdate, UpdateMode,
};
use crate::transport::Connection;

/// Sink for wire transport progress: typed phases via [`ProgressSink`] plus
/// raw side-band channel 2 bytes from the remote.
///
/// Implementations receive the server's progress bytes (typically `\r`-delimited
/// counter lines) through [`Progress::message`]. Typed
/// [`progress::ProgressPhase`](crate::progress::ProgressPhase) updates use the
/// [`crate::progress::ProgressSink`] methods inherited from this supertrait.
pub trait Progress: ProgressSink {
    /// Receive a chunk of progress bytes from side-band channel 2.
    fn message(&mut self, _bytes: &[u8]) {}

    /// When true, fetch omits `no-progress` so the remote may emit channel-2 progress.
    fn remote_sideband_progress(&self) -> bool {
        false
    }

    /// When true, a pack being received is also counted in objects and
    /// reported through [`Progress::received_objects`].
    ///
    /// Counting inflates every entry once more on a background thread (the
    /// receive itself never waits on it), so leave this off unless the counts
    /// are shown.
    fn wants_received_objects(&self) -> bool {
        false
    }

    /// Objects copied or received so far, reported during
    /// [`ReceivingObjects`](crate::progress::ProgressPhase::ReceivingObjects) or
    /// [`CopyingObjects`](crate::progress::ProgressPhase::CopyingObjects) when
    /// [`Progress::wants_received_objects`] is true. Counts may lag the bytes
    /// reported through [`ProgressSink::inc`] slightly on network fetch.
    fn received_objects(&mut self, _counts: crate::progress::ReceivedObjects) {}

    /// Progress of a resumable bundle download (clone with `bundle-uri`) or
    /// upload (push to a `pushBundle` endpoint).
    fn resumable_transfer(&mut self, _event: &crate::resumable::ResumableEvent) {}
}

/// A [`Progress`] / [`ProgressSink`] that discards everything.
pub struct NoProgress;

impl Progress for NoProgress {}

impl ProgressSink for NoProgress {}

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

/// Batch size at which protocol v2 rounds stop doubling and grow by 10%.
const LARGE_FLUSH: usize = 16384;

/// Haves sent since the last new `ACK` after which a v2 fetch gives up and
/// sends `done` (only once the server has acknowledged something).
const MAX_IN_VAIN: usize = 256;

/// Size of the next protocol v2 have batch. Every v2 `fetch` request is
/// stateless, so rounds grow geometrically to bound the number of round trips.
fn next_v2_flush_count(count: usize) -> usize {
    if count < LARGE_FLUSH {
        count * 2
    } else {
        count + count / 10
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
fn read_ack_round(reader: &mut dyn Read, negotiator: &mut SkippingNegotiator<'_>) -> Result<()> {
    let mut reader = reader;
    while let Some(pkt) = pkt_line::read_packet(&mut reader)? {
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

/// Demultiplex the side-band-64k stream after `done`: collect channel-1 pack
/// bytes into `out` (scanning for the `PACK` magic, which may span chunk
/// boundaries), and forward channel-2 progress to `progress`. Channel 3 is a
/// fatal error. Lifted from `read_sideband_pack_until_done`.
fn read_sideband_pack(
    r: &mut dyn Read,
    target: &mut crate::pack_receive::PackReceiveTarget<'_>,
    progress: &mut dyn Progress,
    cancel: &dyn Cancel,
) -> Result<()> {
    crate::pack_receive::read_sideband_pack_to(r, target, progress, cancel)
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
fn append_filter_request_v0(req: &mut Vec<u8>, wire_filter: &str) -> Result<()> {
    pkt_line::write_line_to_vec(req, &format!("filter {wire_filter}"))?;
    Ok(())
}

fn append_shallow_request_v0(
    req: &mut Vec<u8>,
    server_caps: &str,
    local_shallow: &[ObjectId],
    opts: &FetchOptions,
) -> Result<()> {
    let sends_shallow = !local_shallow.is_empty()
        || opts.wire_deepen().is_some()
        || opts
            .deepen_since
            .as_deref()
            .is_some_and(|s| !s.trim().is_empty())
        || opts.deepen_not.iter().any(|s| !s.trim().is_empty());
    if sends_shallow && !server_caps.contains("shallow") {
        return Err(Error::FetchShallowUnsupported);
    }
    for oid in local_shallow {
        pkt_line::write_line_to_vec(req, &format!("shallow {}", oid.to_hex()))?;
    }
    if let Some((depth, relative)) = opts.wire_deepen() {
        if relative && !server_caps.contains("deepen-relative") {
            return Err(Error::FetchDeepenRelativeUnsupported);
        }
        pkt_line::write_line_to_vec(req, &format!("deepen {depth}"))?;
        if relative {
            pkt_line::write_line_to_vec(req, "deepen-relative")?;
        }
    }
    if let Some(since) = opts
        .deepen_since
        .as_deref()
        .filter(|s| !s.trim().is_empty())
    {
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
#[allow(clippy::too_many_arguments)]
fn negotiate_pack(
    local: &Repository,
    conn: &mut dyn Connection,
    wants: &[ObjectId],
    opts: &mut FetchOptions,
    local_shallow: &[ObjectId],
    wire_filter: Option<&str>,
    progress: &mut dyn Progress,
    cancel: &dyn Cancel,
    skip_local_haves: bool,
) -> Result<(Option<std::path::PathBuf>, ShallowUpdate)> {
    crate::trace::trace_span!("grit.fetch.negotiate");
    let want_set: HashSet<ObjectId> = wants.iter().copied().collect();

    let Some(first_want) = wants.first().copied() else {
        return Ok((None, ShallowUpdate::default()));
    };

    // A deepen/shallow request changes the negotiation: the server precedes the
    // pack with a `shallow-info` section, and the client's local history is not a
    // usable negotiation base (its objects bottom out at grafts), so we skip
    // offering `have`s. Mirrors `fetch-pack.c`'s shallow handling.
    let shallow_request = opts.has_deepen_request() || !local_shallow.is_empty();
    let skip_haves = shallow_request || skip_local_haves;

    // Capture the advertised refs before borrowing the writer (avoids aliasing
    // the connection's reader/writer with its accessors). v0/v1 shallow servers
    // append `shallow <oid>` trailer lines to the advertisement; the capability
    // string we read from the advertisement drives `deepen-since`/`deepen-not`.
    let advertised: Vec<(String, ObjectId)> = conn.advertised_refs().to_vec();
    let server_caps: String = conn.capabilities().join(" ");
    validate_fetch_depth_server(opts, Some(&server_caps), None)?;

    // Capability set matching `git fetch-pack`'s first `want` line for v0/v1.
    let mut caps = if opts.remote_progress {
        String::from(" multi_ack_detailed side-band-64k thin-pack include-tag ofs-delta agent=grit")
    } else {
        String::from(
            " multi_ack_detailed side-band-64k thin-pack no-progress include-tag ofs-delta agent=grit",
        )
    };
    if opts.wire_deepen().is_some_and(|(_, relative)| relative) {
        if !server_caps.contains("deepen-relative") {
            return Err(Error::FetchDeepenRelativeUnsupported);
        }
        caps.push_str(" deepen-relative");
    }
    if wire_filter.is_some() {
        caps.push_str(" filter");
    }

    let mut req: Vec<u8> = Vec::new();
    let w0 = format!("want {}{}", first_want.to_hex(), caps);
    pkt_line::write_line_to_vec(&mut req, &w0)?;
    for w in wants.iter().skip(1) {
        pkt_line::write_line_to_vec(&mut req, &format!("want {}", w.to_hex()))?;
    }
    // Match `git fetch-pack`: with a single unique OID, repeat the bare want.
    // git-daemon expects this. (Not done for shallow requests, which append
    // shallow/deepen lines instead.)
    if wants.len() == 1 && !skip_haves {
        pkt_line::write_line_to_vec(&mut req, &format!("want {}", first_want.to_hex()))?;
    }
    if let Some(spec) = wire_filter {
        append_filter_request_v0(&mut req, spec)?;
    }
    append_shallow_request_v0(&mut req, &server_caps, local_shallow, opts)?;
    req.extend_from_slice(b"0000");
    conn.writer().write_all(&req)?;
    conn.writer().flush()?;

    // Build the negotiator from local ref tips (heads, tags, HEAD), peeled to
    // commits, excluding the wants. Advertised tips we already have become
    // `known_common`.
    let mut negotiator = SkippingNegotiator::new(local);
    if !skip_haves {
        add_negotiation_tips(&mut negotiator, &want_set)?;
        // Seed `known_common` only for advertised refs we already track at the same
        // peeled commit. Unchanged refs avoid walking parent chains for every server
        // tip during incremental fetch; moved refs are discovered via `have`s.
        let mut local_by_name: std::collections::HashMap<String, ObjectId> =
            std::collections::HashMap::new();
        for prefix in ["refs/heads/", "refs/tags/"] {
            if let Ok(entries) = crate::refs::list_refs_in(local, prefix) {
                for (name, oid) in entries {
                    local_by_name.insert(name, oid);
                }
            }
        }
        for (name, oid) in &advertised {
            if want_set.contains(oid) {
                continue;
            }
            let Some(local_oid) = local_by_name.get(name) else {
                continue;
            };
            if local_oid != oid {
                continue;
            }
            if let Ok(Some(c)) = negotiator.repo().odb().peel_to_commit(*oid) {
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
    } else if skip_local_haves && conn.stateless_rpc() {
        read_ack_round(conn.reader(), &mut negotiator)?;
    } else if conn.stateless_rpc() {
        // Stateless HTTP completes a POST for the want block; consume its response
        // before the first have round posts again (otherwise the next POST drains it).
        read_ack_round(conn.reader(), &mut negotiator)?;
    }

    // Have/ACK exchange: batch haves, flush, read interleaved ACK rounds.
    let mut count: usize = 0;
    let mut flush_at: usize = INITIAL_FLUSH;
    let mut pending: Vec<u8> = Vec::new();
    let mut flushes: i32 = 0;
    if !skip_haves {
        while let Some(oid) = negotiator.next_have()? {
            poll_cancel(progress, cancel)?;
            pkt_line::write_line_to_vec(&mut pending, &format!("have {}", oid.to_hex()))?;
            count += 1;
            if flush_at <= count {
                pending.extend_from_slice(b"0000");
                conn.writer().write_all(&pending)?;
                conn.writer().flush()?;
                pending.clear();
                flush_at = next_flush_count(count);
                flushes += 1;
                // Duplex: keep one window ahead. Stateless: every POST must be read
                // before the next POST replaces the response.
                if count == INITIAL_FLUSH && !conn.stateless_rpc() {
                    continue;
                }
                read_ack_round(conn.reader(), &mut negotiator)?;
                flushes -= 1;
            }
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

    // Stateless v0 repeats shallow/unshallow lines after `done` before NAK/pack.
    if shallow_request && conn.stateless_rpc() {
        let (sh, unsh) = crate::shallow::read_shallow_info_section(&mut conn.reader())?;
        shallow_update.shallow.extend(sh);
        shallow_update.unshallow.extend(unsh);
    }

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

    let mut pack_receive = crate::pack_receive::TempPackReceive::create(local.odb().objects_dir())?;
    pack_receive.read_sideband(conn.reader(), progress, cancel)?;
    let pack_path = pack_receive.finish()?;
    Ok((pack_path, shallow_update))
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

fn push_unique_prefix(out: &mut Vec<String>, value: &str) {
    if !out.iter().any(|v| v == value) {
        out.push(value.to_owned());
    }
}

/// Ref namespace prefixes to enumerate when advertising refs for fetch (local dir,
/// v2 `ls-refs`, and opportunistic remote-tracking updates).
///
/// Always includes `refs/heads/` and `refs/tags/`. Explicit refspec sources also
/// add literal prefixes (e.g. `refs/heads/main`, `refs/pull/123/head`) so DWIM can
/// resolve `pull/123/head` under `refs/`.
pub(crate) fn fetch_ref_list_prefixes(refspecs: &[String]) -> Vec<String> {
    let mut out = vec!["refs/heads/".to_owned(), "refs/tags/".to_owned()];
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
            push_unique_prefix(&mut out, "HEAD");
            continue;
        }
        if let Some(star) = src.find('*') {
            let prefix = &src[..star];
            if prefix.is_empty() {
                continue;
            }
            if prefix.starts_with("refs/") {
                push_unique_prefix(&mut out, prefix);
            } else {
                push_unique_prefix(&mut out, &format!("refs/heads/{prefix}"));
            }
            continue;
        }
        if src.starts_with("refs/") {
            push_unique_prefix(&mut out, src);
        } else {
            push_unique_prefix(&mut out, &format!("refs/heads/{src}"));
            // Git DWIM: bare names may live directly under `refs/` (pull refs, etc.).
            push_unique_prefix(&mut out, &format!("refs/{src}"));
        }
    }
    out
}

/// `ref-prefix` arguments for a v2 `ls-refs` request, narrowed the way Git does.
///
/// A pattern source asks for its literal prefix (up to the `*`), a full ref name for
/// itself, and a short name for every ref it could resolve to under the rev-parse
/// rules. Object ids need no advertisement. `refs/tags/` is added unless `tags` is
/// [`TagMode::None`](crate::transfer::TagMode::None). Without refspecs this is
/// [`fetch_ref_list_prefixes`] (the heads and tags namespaces).
pub(crate) fn v2_ls_refs_prefixes(
    refspecs: &[String],
    tags: crate::transfer::TagMode,
) -> Vec<String> {
    let mut out = Vec::new();
    for spec in refspecs {
        if spec.starts_with('^') {
            continue;
        }
        let raw = spec.strip_prefix('+').unwrap_or(spec.as_str());
        let src = raw.split_once(':').map_or(raw, |(s, _)| s).trim();
        if src.is_empty() || src == "HEAD" || ObjectId::from_hex(src).is_ok() {
            continue;
        }
        if let Some(star) = src.find('*') {
            let prefix = &src[..star];
            if prefix.starts_with("refs/") {
                push_unique_prefix(&mut out, prefix);
            } else if !prefix.is_empty() {
                push_unique_prefix(&mut out, &format!("refs/heads/{prefix}"));
            }
            continue;
        }
        if src.starts_with("refs/") {
            push_unique_prefix(&mut out, src);
            continue;
        }
        for candidate in [
            src.to_owned(),
            format!("refs/{src}"),
            format!("refs/tags/{src}"),
            format!("refs/heads/{src}"),
            format!("refs/remotes/{src}"),
            format!("refs/remotes/{src}/HEAD"),
        ] {
            push_unique_prefix(&mut out, &candidate);
        }
    }
    if out.is_empty() {
        return fetch_ref_list_prefixes(refspecs);
    }
    if tags != crate::transfer::TagMode::None {
        push_unique_prefix(&mut out, "refs/tags/");
    }
    out
}

/// Parse an `unborn HEAD symref-target:…` line from a v2 `ls-refs` response.
#[must_use]
pub(crate) fn parse_v2_ls_refs_unborn_head_symref(line: &str) -> Option<String> {
    const PREFIX: &str = "unborn HEAD symref-target:";
    let target = line.strip_prefix(PREFIX)?;
    let target = target.split_whitespace().next()?.to_owned();
    crate::refs::is_valid_advertised_symref_target(&target).then_some(target)
}

/// Parse one v2 `ls-refs` advertisement line into `(refname, oid, symref_target)`.
///
/// Lines look like `<oid> <refname>[ symref-target:<t>][ peeled:<oid>]`. Lib-side
/// port of the CLI's `parse_ls_refs_v2_line` (the order of the optional suffixes
/// is whichever the server emits; we scan for both tokens). Returns `None` for a
/// malformed line.
pub(crate) fn parse_ls_refs_v2_line(
    line: &str,
) -> Option<(String, ObjectId, Option<String>, Option<ObjectId>)> {
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
    let advertised_peel = peel_at.and_then(|pos| {
        let tail = &after_oid[pos + PEEL.len()..];
        let end = tail.find(' ').unwrap_or(tail.len());
        ObjectId::from_hex(tail[..end].trim()).ok()
    });
    Some((name, oid, symref_target, advertised_peel))
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
    promisor_reply: Option<&str>,
) -> Result<LsRefsAdvertised> {
    let req = build_v2_ls_refs_request(server_caps, local_odb, tags, refspecs, promisor_reply)?;
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
/// tag-following even when the refspecs name only heads. `promisor_reply`, the
/// accepted promisor remotes, is sent as a `promisor-remote=` capability line.
pub(crate) fn build_v2_ls_refs_request(
    server_caps: &[String],
    local_odb: &crate::odb::Odb,
    tags: crate::transfer::TagMode,
    refspecs: &[String],
    promisor_reply: Option<&str>,
) -> Result<Vec<u8>> {
    let object_format = v2_object_format(server_caps, local_odb);
    let cap_echo = command_cap_echo(server_caps, promisor_reply);

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
    if crate::protocol_v2::server_advertises_ls_refs_unborn(server_caps) {
        pkt_line::write_line(&mut req, "unborn")?;
    }
    pkt_line::write_line(&mut req, "peel")?;

    // Always request `HEAD` so the server advertises its `symref-target`, which
    // drives `FetchOutcome::default_branch` (the wire equivalent of the v0/v1
    // `symref=HEAD:` capability). `HEAD` is dropped from the fetchable ref set.
    pkt_line::write_line(&mut req, "ref-prefix HEAD")?;
    for p in &v2_ls_refs_prefixes(refspecs, tags) {
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
pub(crate) fn parse_v2_ls_refs_response(reader: &mut dyn Read) -> Result<LsRefsAdvertised> {
    // Response: `<oid> <refname>[ symref-target:…][ peeled:…]` lines, flush-terminated.
    let mut advertised: Vec<(String, ObjectId)> = Vec::new();
    let mut head_symref: Option<String> = None;
    let mut head_oid: Option<ObjectId> = None;
    let mut advertised_peel: std::collections::HashMap<String, ObjectId> =
        std::collections::HashMap::new();
    let mut reader = reader;
    loop {
        match pkt_line::read_packet(&mut reader)? {
            None | Some(pkt_line::Packet::Flush) | Some(pkt_line::Packet::Delim) => break,
            Some(pkt_line::Packet::ResponseEnd) => break,
            Some(pkt_line::Packet::Data(line)) => {
                let line = line.trim_end_matches('\n');
                if let Some(msg) = line.strip_prefix("ERR ") {
                    return Err(Error::Message(format!("remote error: {}", msg.trim_end())));
                }
                if let Some(t) = parse_v2_ls_refs_unborn_head_symref(line) {
                    head_symref = Some(t);
                    continue;
                }
                let Some((name, oid, symref_target, peel)) = parse_ls_refs_v2_line(line) else {
                    continue;
                };
                if name.contains("^{") || name.ends_with("^{}") {
                    continue;
                }
                if name == "HEAD" {
                    head_oid = Some(oid);
                    if let Some(t) =
                        symref_target.filter(|t| crate::refs::is_valid_advertised_symref_target(t))
                    {
                        head_symref = Some(t);
                    }
                    // HEAD itself is not a fetchable ref here; refspecs target heads/tags.
                    continue;
                }
                if crate::refs::is_valid_fetch_advertised_ref(&name) {
                    if let Some(p) = peel {
                        advertised_peel.insert(name.clone(), p);
                    }
                    advertised.push((name, oid));
                }
            }
        }
    }
    Ok((advertised, head_symref, advertised_peel, head_oid))
}

/// The ordered local `have` list for a v2 fetch, produced on demand.
///
/// Draining the skipping negotiator walks the whole local history, so only as many haves are
/// taken as the rounds actually send: usually the first batch is enough for the server to be
/// `ready`.
pub(crate) struct V2LocalHaves<'r> {
    negotiator: SkippingNegotiator<'r>,
    taken: Vec<ObjectId>,
    exhausted: bool,
}

impl V2LocalHaves<'_> {
    /// The first `n` haves (fewer when the local history has fewer).
    fn first(&mut self, n: usize) -> Result<&[ObjectId]> {
        while self.taken.len() < n && !self.exhausted {
            match self.negotiator.next_have()? {
                Some(oid) => self.taken.push(oid),
                None => self.exhausted = true,
            }
        }
        Ok(&self.taken[..self.taken.len().min(n)])
    }

    /// Every have, in the same order as [`Self::first`].
    #[cfg(test)]
    fn all(&mut self) -> Result<&[ObjectId]> {
        self.first(usize::MAX)
    }
}

/// Build the ordered `have` candidate list for a v2 fetch from the local ref
/// tips, peeled to commits and skipping those in `exclude`, driven through the
/// [`SkippingNegotiator`]'s skipping schedule.
///
/// `exclude` is normally the wants; a deepen passes none, since the tips it
/// deepens are already local and are the best haves it has. The wire rounds
/// (how many haves per request, when to send `done`) are batched by the caller,
/// which differs between a duplex socket and stateless POSTs.
pub(crate) fn v2_local_haves<'r>(
    local: &'r Repository,
    exclude: &[ObjectId],
) -> Result<V2LocalHaves<'r>> {
    let exclude: HashSet<ObjectId> = exclude.iter().copied().collect();
    let mut negotiator = SkippingNegotiator::new(local);
    add_negotiation_tips(&mut negotiator, &exclude)?;
    Ok(V2LocalHaves {
        negotiator,
        taken: Vec::new(),
        exhausted: false,
    })
}

/// Seed `negotiator` with the local ref tips (heads, remote-tracking refs, tags,
/// HEAD), peeled to commits and excluding `wants`, in a deterministic (hex)
/// order. Remote-tracking tips matter: they usually sit right below what the
/// server is about to send, so offering them keeps the pack minimal.
fn add_negotiation_tips(
    negotiator: &mut SkippingNegotiator<'_>,
    wants: &HashSet<ObjectId>,
) -> Result<()> {
    let local = negotiator.repo();
    let mut tips: Vec<ObjectId> = Vec::new();
    let mut seen_tip: HashSet<ObjectId> = HashSet::new();
    let mut push_tip = |negotiator: &SkippingNegotiator<'_>, oid: ObjectId| {
        if let Ok(Some(c)) = negotiator.repo().odb().peel_to_commit(oid) {
            if !wants.contains(&c) && seen_tip.insert(c) {
                tips.push(c);
            }
        }
    };
    for prefix in [
        "refs/heads/",
        "refs/remotes/",
        "refs/tags/",
        crate::bundle_uri::BUNDLE_REFS_PREFIX,
    ] {
        if let Ok(entries) = crate::refs::list_refs_in(local, prefix) {
            for (_, oid) in entries {
                push_tip(negotiator, oid);
            }
        }
    }
    if let Ok(h) = crate::refs::resolve_ref_in(local, "HEAD") {
        push_tip(negotiator, h);
    }
    tips.sort_unstable();
    for t in tips {
        negotiator.add_tip(t)?;
    }
    Ok(())
}

/// What a v2 `command=fetch` is for, which decides what the request carries.
#[derive(Clone, Copy)]
enum V2FetchMode<'a> {
    /// A ref fetch: offer local haves, and repeat the `promisor-remote` reply
    /// (if any) so a stateless server sees it on the fetch request too.
    Refs {
        /// Accepted promisor remotes, as sent in the `ls-refs` request.
        promisor_reply: Option<&'a str>,
    },
    /// A promisor object-want fetch: no haves (Git's noop negotiator) and no
    /// promisor reply.
    ObjectWants,
}

/// The capability lines echoed on a v2 command request: agent and object-format
/// from the server's advertisement, plus `promisor-remote=<reply>` when the client
/// accepted promisor remotes.
fn command_cap_echo(server_caps: &[String], promisor_reply: Option<&str>) -> Vec<String> {
    let mut echo = protocol_v2::cap_lines_for_command_request(server_caps);
    if let Some(reply) = promisor_reply {
        echo.push(format!(
            "{}={reply}",
            protocol_v2::PROMISOR_REMOTE_CAPABILITY
        ));
    }
    echo
}

/// Run the client side of the `promisor-remote` exchange for a v2 ref fetch.
///
/// Returns `None` when the server advertised no promisor remotes. Accepting needs a
/// partial fetch (a filter, an automatic filter, or a fetch from a remote configured
/// as a promisor remote): otherwise the server could leave out objects this client
/// has no way to get again, and the pack, marked promisor because a remote was
/// accepted, would claim objects no configured promisor can supply. Being a partial
/// clone of some other remote is not enough.
/// Stores `promisor.storeFields` values (unless dry-run), and resolves the filter
/// when [`FetchOptions::auto_filter`] is set.
fn v2_promisor_handshake(
    local: &Repository,
    local_cfg: &crate::config::ConfigSet,
    server_caps: &[String],
    opts: &mut FetchOptions,
) -> Result<Option<crate::promisor_remote::PromisorHandshake>> {
    use crate::net_trace::net_trace;
    use crate::promisor_remote::{PromisorAdvertisement, PromisorHandshake};
    let Some(value) = protocol_v2::server_promisor_remote_info(server_caps) else {
        return Ok(None);
    };
    let advertisement = PromisorAdvertisement::parse(value);
    if advertisement.remotes.is_empty() {
        return Ok(None);
    }
    let auto_filter = opts.filter.is_none() && opts.auto_filter;
    let partial_fetch = opts.filter.is_some()
        || auto_filter
        || opts
            .remote_name
            .as_deref()
            .is_some_and(|name| crate::promisor_remote::is_promisor_remote(local_cfg, name));
    let mut handshake =
        PromisorHandshake::evaluate(local_cfg, &advertisement, partial_fetch, auto_filter);
    if !opts.dry_run {
        handshake.store_fields(local.git_dir(), local_cfg)?;
    }
    if auto_filter {
        opts.filter = handshake.resolved_filter;
    }
    net_trace!(
        opts.network_trace,
        opts.diagnostics.as_ref(),
        "fetch_remote: server advertised {} promisor remote(s), accepted [{}]",
        advertisement.remotes.len(),
        handshake.decision.accepted_names().join(", ")
    );
    Ok(Some(handshake))
}

/// Run a v2 `command=fetch` negotiation over the connection and return the raw
/// pack bytes for `wants`.
///
/// Drives the [`SkippingNegotiator`] lazily, one round per request, framed as
/// v2 (`command=fetch`, capability echo, `0001`, then
/// `thin-pack`/`no-progress`/`ofs-delta`, `want <oid>` lines, `have <oid>` lines,
/// and `done`). v2 servers keep no state between requests, so every round
/// resends the commits the server has acknowledged so far, followed by the next
/// batch of haves (16, then doubling). The walk stops as soon as the server
/// answers `ready`, the negotiator runs out of haves, or [`MAX_IN_VAIN`] haves
/// go unacknowledged after a first `ACK`; the final request carries `done`.
/// Then reads the response sections (`acknowledgments`, optional
/// `shallow-info`, then `wanted-refs`, then `packfile`) and demuxes the
/// side-band-64k pack.
#[allow(clippy::too_many_arguments)]
fn negotiate_pack_v2(
    local: &Repository,
    conn: &mut dyn Connection,
    server_caps: &[String],
    wants: &[ObjectId],
    deepen: &V2DeepenArgs,
    wire_filter: Option<&str>,
    progress: &mut dyn Progress,
    cancel: &dyn Cancel,
    mode: V2FetchMode<'_>,
) -> Result<(Option<std::path::PathBuf>, ShallowUpdate)> {
    if wants.is_empty() {
        return Ok((None, ShallowUpdate::default()));
    }
    let (skip_local_haves, promisor_reply) = match mode {
        V2FetchMode::Refs { promisor_reply } => (false, promisor_reply),
        V2FetchMode::ObjectWants => (true, None),
    };
    let local_odb = local.odb();
    let object_format = v2_object_format(server_caps, local_odb);
    let cap_echo = command_cap_echo(server_caps, promisor_reply);
    let sideband_all = protocol_v2::fetch_supports_sideband_all(server_caps);

    // The ordered `have` list is produced lazily from local ref tips. Empty for a
    // promisor object-want fetch (Git `negotiationAlgorithm=noop`). A
    // deepen/shallow request negotiates too: the walk stops at local shallow
    // commits, and the server bounds the haves by the `shallow` lines we send, so
    // a deepen only ships history the client lacks. Its wants are usually already
    // local (the tips being deepened) and are offered as haves as well.
    let mut haves = if skip_local_haves {
        None
    } else {
        let exclude: &[ObjectId] = if deepen.is_shallow_request() {
            &[]
        } else {
            wants
        };
        Some(v2_local_haves(local, exclude)?)
    };

    let mut pack_receive = crate::pack_receive::TempPackReceive::create(local_odb.objects_dir())?;
    let mut shallow_update = ShallowUpdate::default();
    let mut common: Vec<ObjectId> = Vec::new();
    let mut common_set: HashSet<ObjectId> = HashSet::new();
    let mut batch = INITIAL_FLUSH;
    let mut in_vain: usize = 0;
    let mut seen_ack = false;
    let mut round_haves: Vec<ObjectId> = Vec::new();
    loop {
        round_haves.clear();
        round_haves.extend_from_slice(&common);
        let added = match haves.as_mut() {
            Some(haves) => {
                poll_cancel(progress, cancel)?;
                let previous = haves.taken.len();
                haves.first(previous.saturating_add(batch))?;
                round_haves.extend_from_slice(&haves.taken[previous..]);
                haves.taken.len() - previous
            }
            None => 0,
        };
        in_vain += added;
        let done = added == 0 || (seen_ack && in_vain >= MAX_IN_VAIN);
        write_v2_fetch_request(
            conn.writer(),
            &object_format,
            &cap_echo,
            wants,
            &round_haves,
            sideband_all,
            deepen,
            wire_filter,
            done,
        )?;
        if done {
            break;
        }
        match read_v2_acknowledgments(conn.reader(), sideband_all)? {
            // Not ready: record the new common commits and send the next round.
            // The server re-ACKs every replayed common commit, so only a newly
            // common commit counts as progress toward the in-vain cutoff.
            Some(round) if !round.ready => {
                for oid in round.common {
                    if let Some(haves) = haves.as_mut() {
                        haves.negotiator.ack(oid)?;
                    }
                    if common_set.insert(oid) {
                        common.push(oid);
                        in_vain = 0;
                    }
                    seen_ack = true;
                }
                batch = next_v2_flush_count(batch);
            }
            // `ready` (or no acknowledgments section): the pack follows in
            // this same response after a delimiter.
            _ => break,
        }
    }
    read_v2_fetch_pack_response(
        conn.reader(),
        &mut Vec::new(),
        Some(&mut pack_receive),
        &mut shallow_update,
        progress,
        cancel,
        sideband_all,
    )?;
    Ok((pack_receive.finish()?, shallow_update))
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
    /// When true, send v0/v1/v2 `deepen-relative` after `deepen`.
    pub(crate) deepen_relative: bool,
    /// `deepen-since <unix-ts>`.
    pub(crate) deepen_since: Option<String>,
    /// `deepen-not <ref>` exclusions.
    pub(crate) deepen_not: Vec<String>,
    /// When true, omit the v2 `no-progress` fetch argument.
    pub(crate) remote_progress: bool,
}

impl V2DeepenArgs {
    /// Build the v2 deepen args from the fetch options and the local shallow file,
    /// translating `--unshallow` into the `INFINITE_DEPTH` deepen Git uses.
    pub(crate) fn from_opts(opts: &FetchOptions, local_shallow: &[ObjectId]) -> Self {
        let (depth, deepen_relative) = opts
            .wire_deepen()
            .map(|(d, r)| (Some(d), r))
            .unwrap_or((None, false));
        Self {
            local_shallow: local_shallow.to_vec(),
            depth,
            deepen_relative,
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
            remote_progress: opts.remote_progress,
        }
    }

    /// Whether any deepen/shallow argument is present (drives `shallow-info`
    /// handling and whether the wanted tips may also be offered as haves).
    pub(crate) fn is_shallow_request(&self) -> bool {
        self.depth.is_some()
            || self.deepen_relative
            || self.deepen_since.is_some()
            || !self.deepen_not.is_empty()
            || !self.local_shallow.is_empty()
    }
}
#[expect(clippy::too_many_arguments)]
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
    wire_filter: Option<&str>,
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
    if !deepen.remote_progress {
        pkt_line::write_line(&mut req, "no-progress")?;
    }
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
    if deepen.deepen_relative {
        pkt_line::write_line(&mut req, "deepen-relative")?;
    }
    if let Some(since) = &deepen.deepen_since {
        pkt_line::write_line(&mut req, &format!("deepen-since {since}"))?;
    }
    for excl in &deepen.deepen_not {
        pkt_line::write_line(&mut req, &format!("deepen-not {excl}"))?;
    }
    if let Some(spec) = wire_filter {
        pkt_line::write_line(&mut req, &format!("filter {spec}"))?;
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
    /// Commits the server acknowledged as common (`ACK <oid>`), in wire order.
    pub(crate) common: Vec<ObjectId>,
}

/// Read a v2 `acknowledgments` section header and its `ACK`/`NAK`/`ready` lines.
///
/// Returns `Some(round)` for an `acknowledgments` section (with `ready` set when
/// the server is ready to send the pack), or `None` when the server skipped the
/// section and started a different one (e.g. went straight to `packfile`) — in
/// which case the header has been consumed and the caller proceeds to read the
/// pack response directly. Lifted from the CLI's `read_v2_acknowledgments`.
pub(crate) fn read_v2_acknowledgments(
    reader: &mut dyn Read,
    sideband_all: bool,
) -> Result<Option<V2AckRound>> {
    let mut reader = reader;
    let hdr = match crate::shallow::read_v2_fetch_primary_line(&mut reader, sideband_all)? {
        Some(s) => s,
        None => {
            return Ok(Some(V2AckRound {
                ready: false,
                common: Vec::new(),
            }))
        }
    };
    let hdr = hdr.as_str();
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
    let mut common = Vec::new();
    loop {
        match pkt_line::read_packet(&mut reader)? {
            Some(pkt_line::Packet::Delim) | Some(pkt_line::Packet::Flush) | None => break,
            Some(pkt_line::Packet::ResponseEnd) => break,
            Some(pkt_line::Packet::Data(payload)) => {
                let Some(ln) = crate::shallow::v2_primary_line_from_pkt(payload, sideband_all)?
                else {
                    continue;
                };
                if ln == "NAK" {
                    continue;
                }
                if let Some(hex) = ln.strip_prefix("ACK ") {
                    let oid = ObjectId::from_hex(hex.trim()).map_err(|_| {
                        Error::Message(format!("unexpected acknowledgment line: '{ln}'"))
                    })?;
                    common.push(oid);
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
        }
    }
    Ok(Some(V2AckRound { ready, common }))
}

/// Whether `line` is a v2 `wanted-refs` (or ls-refs) advertisement row:
/// `<oid> <refname>[ …attributes…]`.
fn v2_wanted_ref_ad_line(line: &str) -> bool {
    let line = line.trim_end();
    let Some((oid_hex, rest)) = line.split_once(' ') else {
        return false;
    };
    if ObjectId::from_hex(oid_hex.trim()).is_err() {
        return false;
    }
    !rest.trim().is_empty()
}

/// Read a v2 `command=fetch` response: capture the `shallow-info` section's
/// `shallow`/`unshallow` lines into `shallow_out`, skip the other non-pack
/// sections (`acknowledgments`/`shallow-info`/`wanted-refs`/`packfile-uris`), and demux the
/// side-band-64k pack from the `packfile` section into `out`. Lifted from the
/// CLI's `read_v2_fetch_pack_response`, extended to surface shallow updates.
pub(crate) fn read_v2_fetch_pack_response(
    reader: &mut dyn Read,
    out: &mut Vec<u8>,
    pack_sink: Option<&mut crate::pack_receive::TempPackReceive>,
    shallow_out: &mut ShallowUpdate,
    progress: &mut dyn Progress,
    cancel: &dyn Cancel,
    sideband_all: bool,
) -> Result<()> {
    loop {
        let hdr = loop {
            match pkt_line::read_packet(&mut &mut *reader)? {
                Some(pkt_line::Packet::Data(payload)) => {
                    if let Some(line) =
                        crate::shallow::v2_primary_line_from_pkt(payload, sideband_all)?
                    {
                        break line;
                    }
                }
                Some(pkt_line::Packet::Flush) | None => return Ok(()),
                Some(pkt_line::Packet::Delim) => continue,
                Some(other) => {
                    return Err(Error::Message(format!(
                        "unexpected v2 fetch response: {other:?}"
                    )))
                }
            }
        };
        let hdr = hdr.as_str();
        if let Some(msg) = hdr.strip_prefix("ERR ") {
            return Err(Error::Message(format!("remote error: {}", msg.trim_end())));
        }
        match hdr {
            h if h == crate::protocol_v2::FetchResponseSection::ShallowInfo.header() => {
                // Capture the shallow/unshallow boundary updates. The section is
                // delim-terminated (before the `packfile` header), which
                // `read_shallow_info_section` stops at, leaving the header intact.
                let (sh, unsh) =
                    crate::shallow::read_shallow_info_section_sideband(&mut *reader, sideband_all)?;
                shallow_out.shallow.extend(sh);
                shallow_out.unshallow.extend(unsh);
            }
            h if h == crate::protocol_v2::FetchResponseSection::Acknowledgments.header()
                || h == crate::protocol_v2::FetchResponseSection::WantedRefs.header()
                || h == crate::protocol_v2::FetchResponseSection::PackfileUris.header() =>
            {
                skip_v2_section_until_boundary(&mut *reader, sideband_all)?;
            }
            h if h == crate::protocol_v2::FetchResponseSection::Packfile.header() => {
                // The `packfile` section body is side-band-64k framed; reuse the
                // shared demuxer (channel 1 = pack, channel 2 = progress, 3 = err).
                if let Some(sink) = pack_sink {
                    sink.read_sideband(&mut *reader, progress, cancel)?;
                } else {
                    let mut target = crate::pack_receive::PackReceiveTarget::Memory(out);
                    read_sideband_pack(&mut *reader, &mut target, progress, cancel)?;
                }
                return Ok(());
            }
            other if v2_wanted_ref_ad_line(other) => {
                // `wanted-refs` payload line (some servers omit the section header).
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
fn skip_v2_section_until_boundary(reader: &mut dyn Read, sideband_all: bool) -> Result<()> {
    loop {
        match pkt_line::read_packet(&mut &mut *reader)? {
            None | Some(pkt_line::Packet::Flush) | Some(pkt_line::Packet::Delim) => return Ok(()),
            Some(pkt_line::Packet::ResponseEnd) => return Ok(()),
            Some(pkt_line::Packet::Data(payload)) => {
                let _ = crate::shallow::v2_primary_line_from_pkt(payload, sideband_all)?;
            }
        }
    }
}

/// Pack ingest target for network fetch [`FetchOptions::dry_run`]: objects are classified but not published.
#[allow(clippy::large_enum_variant)]
enum FetchDryRunIngest {
    Quarantine(crate::receive_quarantine::ReceiveQuarantine),
    Memory {
        objects_dir: std::path::PathBuf,
        pack: std::sync::Arc<crate::objects::odb::store::MemoryStore>,
        read: std::sync::Arc<dyn crate::objects::odb::store::ObjectStore>,
    },
}

impl FetchDryRunIngest {
    fn create(local_odb: &crate::odb::Odb, git_dir: &Path) -> Result<Self> {
        use crate::objects::odb::store::MemoryStore;
        if local_odb.uses_files_primary() {
            Ok(Self::Quarantine(
                crate::receive_quarantine::ReceiveQuarantine::create(local_odb, git_dir)?,
            ))
        } else {
            let pack = std::sync::Arc::new(MemoryStore::new(local_odb.hash_algo()));
            Ok(Self::Memory {
                objects_dir: local_odb.objects_dir().to_path_buf(),
                pack: pack.clone(),
                read: local_odb.read_store(),
            })
        }
    }

    fn odb(&self) -> crate::odb::Odb {
        match self {
            Self::Quarantine(q) => q.odb(),
            Self::Memory {
                objects_dir,
                pack,
                read,
            } => crate::odb::OdbBuilder::files(objects_dir)
                .primary(pack.clone())
                .push_read_source(read.clone())
                .alternates(false)
                .build(),
        }
    }
}

/// Fetch from a remote over a live [`Connection`], driving the upload-pack
/// negotiation and writing the resulting tracking-ref updates into
/// `local_git_dir`.
///
/// The flow mirrors local fetch, but the remote ref list comes from the
/// connection's advertisement, objects arrive over the wire as a negotiated pack,
/// and the local repo is opened to classify ancestry. Reuses refspec matching,
/// tag-mode, prune, and classification helpers shared with other fetch paths.
///
/// Handles protocol v0, v1, and v2. For a v2 connection the ref map is recovered
/// via a `command=ls-refs` round (no refs are advertised on connect) and the
/// pack is negotiated with `command=fetch`; v0/v1 use the connect-time
/// advertisement and the classic `want`/`have`/`done` exchange.
///
/// # Errors
///
/// The local repository is opened with [`FetchOptions::environment`], so objects land in its
/// `GIT_OBJECT_DIRECTORY` when that is set.
///
/// Returns an error if `local_git_dir` is not a repository, if a refspec is invalid, if the
/// negotiation or pack ingest fails, or on ref/odb I/O failure.
pub fn fetch_remote(
    local_git_dir: &Path,
    conn: &mut dyn Connection,
    opts: &mut FetchOptions,
    progress: &mut dyn Progress,
    cancel: &dyn Cancel,
) -> Result<FetchOutcome> {
    let local = open_fetch_target(local_git_dir, opts)?;
    fetch_remote_into(&local, conn, opts, progress, cancel)
}

/// Open the repository a git-dir based fetch writes into, honouring the fetch's environment.
pub(crate) fn open_fetch_target(local_git_dir: &Path, opts: &FetchOptions) -> Result<Repository> {
    Repository::open(
        local_git_dir,
        &RepositoryOptions::with_environment(opts.environment.clone()),
    )
}

/// Like [`fetch_remote`], reading and writing objects through `local`'s object store.
///
/// Received packs are ingested into `local`'s primary backend, including a custom
/// [`OdbBuilder::primary`](crate::odb::OdbBuilder::primary) store. Set
/// [`FetchOptions::pack_spool_only`](crate::transfer::FetchOptions::pack_spool_only) to keep the
/// negotiated pack on disk and ingest it with [`crate::index_pack::ingest_received_pack_path`]
/// or [`crate::unpack_objects::ingest_pack_into`].
pub fn fetch_remote_into(
    local: &Repository,
    conn: &mut dyn Connection,
    opts: &mut FetchOptions,
    progress: &mut dyn Progress,
    cancel: &dyn Cancel,
) -> Result<FetchOutcome> {
    use crate::net_trace::net_trace;
    let local_git_dir = local.git_dir();
    // Wall-clock since fetch_remote entry, included in phase-boundary traces so
    // `GRIT_NET_DEBUG=1` shows where time goes (see the "matched"/"done" lines).
    let t_begin = std::time::Instant::now();
    net_trace!(
        opts.network_trace,
        opts.diagnostics.as_ref(),
        "fetch_remote: begin — protocol v{}, {} refspec(s), tags={:?}, depth={:?}",
        conn.protocol_version(),
        opts.refspecs.len(),
        opts.tags,
        opts.fetch_depth
    );
    validate_fetch_depth_local(local_git_dir, opts)?;
    let local_cfg = crate::config::ConfigSet::load(&opts.environment, Some(local_git_dir), true)?;
    if opts.filter.is_none() {
        if let Some(remote) = opts.remote_name.as_deref() {
            opts.filter = crate::partial_clone::stored_remote_filter(&local_cfg, remote);
            opts.auto_filter |=
                crate::partial_clone::stored_remote_filter_is_auto(&local_cfg, remote);
        }
    }
    if !opts.object_wants.is_empty() {
        return fetch_remote_object_wants(local, conn, opts, progress, cancel);
    }
    let mut promisor_handshake: Option<crate::promisor_remote::PromisorHandshake> = None;
    let local_odb_for_ls = local.odb();

    // 1. Remote refs + default branch.
    //
    // For protocol v2 the connect-time advertisement carries no refs (only the
    // capability block); we obtain them now with an `ls-refs` command, derived
    // from the fetch refspecs. For v0/v1 they come from the connect-time
    // advertisement directly.
    let (remote_refs, default_branch, v2_caps, advertised_peel, remote_head_oid) = if conn
        .protocol_version()
        >= 2
    {
        let caps: Vec<String> = conn.capabilities().to_vec();
        promisor_handshake = v2_promisor_handshake(local, &local_cfg, &caps, opts)?;
        let promisor_reply = promisor_handshake
            .as_ref()
            .and_then(|h| h.decision.reply_value());
        let (refs, head_symref, peel, head_oid) = v2_ls_refs(
            conn,
            &caps,
            local_odb_for_ls,
            opts.tags,
            &opts.refspecs,
            promisor_reply.as_deref(),
        )?;
        let default_branch =
            head_symref.map(|t| t.strip_prefix("refs/heads/").unwrap_or(&t).to_owned());
        (refs, default_branch, Some(caps), peel, head_oid)
    } else {
        let default_branch = conn
            .head_symref()
            .map(|t| t.strip_prefix("refs/heads/").unwrap_or(t).to_owned());
        let advertised_peel = conn.advertised_tag_peel().cloned().unwrap_or_default();
        let remote_head_oid =
            crate::transfer::remote_head_oid_from_advertised(conn.advertised_refs());
        let remote_refs: Vec<(String, ObjectId)> = conn
            .advertised_refs()
            .iter()
            .filter(|(n, _)| {
                n != "HEAD" && !n.ends_with("^{}") && crate::refs::is_valid_fetch_advertised_ref(n)
            })
            .cloned()
            .collect();
        (
            remote_refs,
            default_branch,
            None,
            advertised_peel,
            remote_head_oid,
        )
    };
    net_trace!(
        opts.network_trace,
        opts.diagnostics.as_ref(),
        "fetch_remote: remote advertised {} ref(s){}",
        remote_refs.len(),
        v2_caps
            .as_ref()
            .map(|_| " (via v2 ls-refs)")
            .unwrap_or(" (v0/v1 advertisement)")
    );

    if opts.initial_remote_fetch
        && remote_refs.is_empty()
        && !crate::transport::connection_handshake_complete(conn)
    {
        return Err(Error::RemoteUnreadable {
            url: None,
            reason: crate::error::RemoteUnreadableReason::NoHandshake,
        });
    }

    let conn_caps: Vec<String> = conn.capabilities().to_vec();
    let remote_algo = crate::transfer::remote_hash_algo_from_advertisement(
        &[v2_caps.as_deref().unwrap_or(&[]), conn_caps.as_slice()],
        &remote_refs,
    );
    let reopened =
        crate::transfer::sync_empty_destination_object_format(local_git_dir, remote_algo, opts)?
            .then(|| crate::transfer::reopen_after_format_change(local))
            .transpose()?;
    let local = reopened.as_ref().unwrap_or(local);
    let local_git_dir = local.git_dir();
    let local_odb = local.odb();
    if opts.filter.is_some() && !local_odb.uses_files_primary() {
        return Err(Error::UnsupportedObjectStore {
            operation: "partial fetch",
        });
    }

    let crate::transfer::FetchMatchSet {
        mut matched,
        positive,
        negatives,
        explicit_refspec_fetch,
    } = crate::transfer::build_fetch_matched_set(opts, &remote_refs, remote_head_oid)?;
    let mut matched_oids: HashSet<ObjectId> = matched.iter().map(|m| m.oid).collect();
    let mut seen_remote_ref: HashSet<String> =
        matched.iter().map(|m| m.remote_ref.clone()).collect();

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
    let tags_present = if opts.tags == crate::transfer::TagMode::Following {
        tags_already_local(local.refs(), &remote_refs)
    } else {
        HashSet::new()
    };
    let following_only = add_wire_tags(
        opts.tags,
        &remote_refs,
        &advertised_peel,
        &negatives,
        &tags_present,
        &mut matched,
        &mut matched_oids,
        &mut seen_remote_ref,
    );
    let fetch_commit_oids: HashSet<ObjectId> = matched
        .iter()
        .filter(|m| !m.is_tag)
        .map(|m| m.oid)
        .collect();
    let mut following_tag_wants =
        following_tag_wants_with_local_peel(&matched, &following_only, local_odb)?;

    // The client's current shallow grafts (drives the wire `shallow <oid>` lines
    // and the "this is a shallow request" decisions in the negotiators).
    let local_shallow = crate::shallow::load_shallow_oids(local_git_dir)?;
    let shallow_request = opts.has_deepen_request() || !local_shallow.is_empty();
    if shallow_request {
        let local_shallow_set: HashSet<ObjectId> = local_shallow.iter().copied().collect();
        let peel_set = following_peel_commit_set_for_shallow(
            local_odb,
            &fetch_commit_oids,
            opts.absolute_depth_for_tags(),
            &local_shallow_set,
        )?;
        following_tag_wants.extend(following_tag_wants_peel_among_fetched_commits(
            &matched,
            &following_only,
            &peel_set,
        ));
    }

    // 4. Wants. Normally the matched oids that are absent locally. For a
    // deepen/`--unshallow` request the wanted tips may already be present (a prior
    // shallow fetch landed them); we must still `want` them so the server fills in
    // the now-reachable ancestors past the old boundary.
    let wants: Vec<ObjectId> = if shallow_request {
        matched_oids
            .iter()
            .copied()
            .filter(|oid| !following_only.contains(oid) || following_tag_wants.contains(oid))
            .collect()
    } else {
        matched_oids
            .iter()
            .copied()
            .filter_map(|oid| {
                if following_only.contains(&oid) {
                    return following_tag_wants.contains(&oid).then_some(Ok(oid));
                }
                match local_odb.contains(&oid) {
                    Ok(true) => None,
                    Ok(false) => Some(Ok(oid)),
                    Err(err) => Some(Err(err)),
                }
            })
            .collect::<Result<_>>()?
    };

    // Shallow-boundary updates the server reports (`shallow`/`unshallow`), applied
    // to the local `shallow` file and surfaced in the outcome.
    let mut shallow_update = ShallowUpdate::default();
    let mut pack_oids: HashSet<ObjectId> = HashSet::new();
    let mut transfer = crate::transfer::TransferStats::default();

    net_trace!(
        opts.network_trace,
        opts.diagnostics.as_ref(),
        "fetch_remote: {} matched ref(s), want {} object(s){} [+{:?} since begin]",
        matched.len(),
        wants.len(),
        if shallow_request {
            " (shallow request)"
        } else {
            ""
        },
        t_begin.elapsed()
    );

    let mut dry_run_ingest: Option<FetchDryRunIngest> = None;

    let v0_caps = if v2_caps.is_none() {
        Some(conn.capabilities().join(" "))
    } else {
        None
    };
    let (wire_filter, filter_sent) = crate::partial_clone::wire_filter_for_capabilities(
        opts.filter.as_ref(),
        v2_caps.as_deref(),
        v0_caps.as_deref(),
    );
    // Match Git: a requested/stored partial-clone filter marks the received pack
    // promisor even when the server lacks `filter` and fetches unfiltered. So does
    // accepting a promisor remote: the server may then leave out objects that
    // remote holds.
    let promisor_reply = promisor_handshake
        .as_ref()
        .and_then(|h| h.decision.reply_value());
    opts.mark_received_pack_promisor = opts.filter.is_some() || promisor_reply.is_some();
    let wire_filter = wire_filter.as_deref();

    if wants.is_empty() {
        crate::transport::send_client_flush(conn)?;
    } else {
        net_trace!(
            opts.network_trace,
            opts.diagnostics.as_ref(),
            "fetch_remote: negotiating + fetching pack…"
        );
        let (pack_path, su) = if let Some(caps) = v2_caps.as_ref() {
            validate_fetch_depth_server(opts, None, Some(caps))?;
            let deepen = V2DeepenArgs::from_opts(opts, &local_shallow);
            negotiate_pack_v2(
                local,
                conn,
                caps,
                &wants,
                &deepen,
                wire_filter,
                progress,
                cancel,
                V2FetchMode::Refs {
                    promisor_reply: promisor_reply.as_deref(),
                },
            )?
        } else {
            negotiate_pack(
                local,
                conn,
                &wants,
                opts,
                &local_shallow,
                wire_filter,
                progress,
                cancel,
                false,
            )?
        };
        shallow_update = su;
        net_trace!(
            opts.network_trace,
            opts.diagnostics.as_ref(),
            "fetch_remote: received pack, ingesting via index-pack…"
        );
        if let Some(path) = pack_path {
            if opts.pack_spool_only {
                net_trace!(
                    opts.network_trace,
                    opts.diagnostics.as_ref(),
                    "fetch_remote: pack spooled to {} (pack_spool_only)",
                    path.display()
                );
                return Ok(FetchOutcome {
                    updates: Vec::new(),
                    default_branch: None,
                    new_shallow: crate::shallow::dedupe_shallow_list(shallow_update.shallow),
                    new_unshallow: shallow_update.unshallow,
                    transfer: crate::transfer::TransferStats::default(),
                    filter_sent,
                    promisor: promisor_handshake,
                });
            }
            let pack_config =
                crate::config::ConfigSet::load(&opts.environment, Some(local_git_dir), true)?;
            let index_threads = crate::parallel::index_pack_threads_for_ingest(
                opts.thread_policy,
                &pack_config,
                opts.index_pack_threads,
            )?;
            if opts.dry_run && dry_run_ingest.is_none() {
                dry_run_ingest = Some(FetchDryRunIngest::create(local_odb, local_git_dir)?);
            }
            let ingested = if let Some(target) = dry_run_ingest.as_ref() {
                let q_odb = target.odb();
                crate::index_pack::ingest_received_pack_path_with_progress(
                    path,
                    &q_odb,
                    &crate::index_pack::IngestPackOptions {
                        fix_thin: true,
                        skip_post_index_verify: true,
                        threads: index_threads,
                        merged_config: Some(pack_config),
                        environment: opts.environment.clone(),
                        mark_promisor_pack: opts.mark_received_pack_promisor,
                        trusted_index_rows: None,
                    },
                    Some(progress),
                    Some(cancel),
                )?
            } else {
                crate::index_pack::ingest_received_pack_path_with_progress(
                    path,
                    local_odb,
                    &crate::index_pack::IngestPackOptions {
                        fix_thin: true,
                        skip_post_index_verify: true,
                        threads: index_threads,
                        merged_config: Some(pack_config),
                        environment: opts.environment.clone(),
                        mark_promisor_pack: opts.mark_received_pack_promisor,
                        trusted_index_rows: None,
                    },
                    Some(progress),
                    Some(cancel),
                )?
            };
            transfer = crate::transfer::TransferStats::from_ingested_pack(&ingested);
            pack_oids = ingested.object_ids;
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
    if v2_caps.is_some() && !wants.is_empty() {
        conn.finish_send();
    }

    // For TagMode::Following, prune tags whose target did not arrive in the
    // pack (now resolvable against the local odb, which holds the fetched
    // objects). All/None already handled; Following kept only when reachable.
    if opts.tags == crate::transfer::TagMode::Following {
        let t = std::time::Instant::now();
        let tag_odb = dry_run_ingest
            .as_ref()
            .map(FetchDryRunIngest::odb)
            .unwrap_or_else(|| local_odb.clone());
        retain_following_tags(
            &tag_odb,
            &mut matched,
            &pack_oids,
            &crate::shallow::load_shallow_boundaries(local_git_dir),
        )?;
        net_trace!(
            opts.network_trace,
            opts.diagnostics.as_ref(),
            "fetch_remote: retain_following_tags {:?}",
            t.elapsed()
        );
    }

    // 5. Classify + apply ref updates (ancestry via the now-populated local repo).
    let t_apply = std::time::Instant::now();
    let classify_odb = dry_run_ingest.as_ref().map(FetchDryRunIngest::odb);
    let classify_repo = (!opts.dry_run).then_some(local);
    let d_repo_open = t_apply.elapsed();

    let mut updates: Vec<RefUpdate> = Vec::new();

    let t_prune = std::time::Instant::now();
    if opts.prune {
        prune_tracking_refs(local, &positive, &remote_refs, opts.dry_run, &mut updates)?;
    }
    let d_prune = t_prune.elapsed();

    let mut store_batch: Vec<(String, crate::refs::store::RawRef)> =
        Vec::with_capacity(matched.len());

    // Load `packed-refs` once for the whole apply. Resolving each ref's previous
    // value (and the per-write namespace-conflict check) would otherwise re-read
    // and re-scan the entire file per ref — O(refs × packed-refs size), which
    // dominates fetches on repositories with tens of thousands of packed refs
    // even when no objects are transferred. Refs not kept in the git directory
    // are resolved through the repository's store instead.
    let t_packed = std::time::Instant::now();
    let packed = if local.refs_on_disk() {
        Some(crate::refs::PackedRefs::load(local_git_dir)?)
    } else {
        None
    };
    let d_packed = t_packed.elapsed();

    // Per-sub-step timing accumulators for the apply loop, surfaced via the
    // env-gated "done" trace so the dominant cost is visible without a profiler.
    let mut d_resolve = std::time::Duration::ZERO;
    let mut d_classify = std::time::Duration::ZERO;
    let mut d_write = std::time::Duration::ZERO;
    let mut n_written = 0usize;
    let mut n_ancestry = 0usize; // refs whose old != new → classify walks ancestry

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

        let t = std::time::Instant::now();
        let old = match &packed {
            Some(packed) => crate::refs::resolve_ref_cached(local_git_dir, local_ref, packed)
                .ok()
                .flatten(),
            None => local.resolve_ref_oid(local_ref).ok(),
        };
        d_resolve += t.elapsed();

        if old.as_ref().is_some_and(|o| o != &m.oid) {
            n_ancestry += 1;
        }

        let t = std::time::Instant::now();
        let mode = classify_update(
            old.as_ref(),
            &m.oid,
            m.force,
            m.dest_is_tag,
            classify_repo,
            classify_odb.as_ref(),
        );
        d_classify += t.elapsed();

        let write = matches!(
            mode,
            UpdateMode::New | UpdateMode::FastForward | UpdateMode::Forced
        );
        crate::transfer::enforce_fetch_ref_update(
            local_ref,
            old,
            m.oid,
            mode,
            opts,
            explicit_refspec_fetch,
            classify_repo,
        )?;
        if write && !opts.dry_run {
            if !crate::refs::is_valid_storable_ref_name(local_ref) {
                updates.push(RefUpdate {
                    remote_ref: m.remote_ref.clone(),
                    local_ref: Some(local_ref.clone()),
                    old_oid: old,
                    new_oid: Some(m.oid),
                    mode,
                    note: Some("skipped (invalid ref name)".to_owned()),
                });
                continue;
            }
            store_batch.push((local_ref.clone(), crate::refs::store::RawRef::Direct(m.oid)));
            n_written += 1;
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

    if !opts.dry_run && !store_batch.is_empty() {
        let t = std::time::Instant::now();
        crate::transfer::apply_fetch_ref_updates(
            local,
            store_batch,
            &mut updates,
            crate::transfer::FetchRefRejectHint {
                prune: opts.prune,
                positive: &positive,
                remote_refs: &remote_refs,
                remote_name: opts.remote_name.as_deref(),
            },
            opts.initial_remote_fetch,
        )?;
        d_write += t.elapsed();
    }

    net_trace!(
        opts.network_trace,
        opts.diagnostics.as_ref(),
        "fetch_remote: apply {:?} — repo_open {:?}, prune {:?}, packed_load {:?}, \
         resolve {:?}, classify {:?} ({} ancestry-checked), write {:?} ({} written)",
        t_apply.elapsed(),
        d_repo_open,
        d_prune,
        d_packed,
        d_resolve,
        d_classify,
        n_ancestry,
        d_write,
        n_written
    );

    net_trace!(
        opts.network_trace,
        opts.diagnostics.as_ref(),
        "fetch_remote: done — {} ref update(s){} [+{:?} total]",
        updates.len(),
        default_branch
            .as_deref()
            .map(|b| format!(", default branch '{b}'"))
            .unwrap_or_default(),
        t_begin.elapsed()
    );
    finish_initial_remote_fetch_layout(local, opts, default_branch.as_deref())?;
    if let Some(remote) = opts.remote_name.as_deref() {
        ensure_remote_tracking_head_symref(local, remote, default_branch.as_deref(), opts.dry_run)?;
    }
    crate::transfer::finish_anonymous_url_fetch(
        local_git_dir,
        opts,
        explicit_refspec_fetch,
        default_branch.as_deref(),
        &remote_refs,
        &matched,
    )?;
    Ok(FetchOutcome {
        updates,
        default_branch,
        new_shallow: crate::shallow::dedupe_shallow_list(shallow_update.shallow),
        new_unshallow: shallow_update.unshallow,
        transfer,
        filter_sent,
        promisor: promisor_handshake,
    })
}

/// Promisor on-demand fetch: negotiate a pack for [`FetchOptions::object_wants`] only.
fn fetch_remote_object_wants(
    local: &Repository,
    conn: &mut dyn Connection,
    opts: &mut FetchOptions,
    progress: &mut dyn Progress,
    cancel: &dyn Cancel,
) -> Result<FetchOutcome> {
    use crate::net_trace::net_trace;

    let t_begin = std::time::Instant::now();
    let local_git_dir = local.git_dir();
    let local_odb = local.odb();
    let wants = local_odb.missing_stored_oids(opts.object_wants.iter().copied())?;
    if wants.is_empty() {
        return Ok(FetchOutcome {
            updates: Vec::new(),
            default_branch: None,
            new_shallow: Vec::new(),
            new_unshallow: Vec::new(),
            transfer: crate::transfer::TransferStats::default(),
            filter_sent: false,
            promisor: None,
        });
    }

    let v2_caps = if conn.protocol_version() >= 2 {
        Some(conn.capabilities().to_vec())
    } else {
        None
    };
    let v0_caps = if v2_caps.is_none() {
        Some(conn.capabilities().join(" "))
    } else {
        None
    };
    let (wire_filter, filter_sent) = crate::partial_clone::wire_filter_for_capabilities(
        opts.filter.as_ref(),
        v2_caps.as_deref(),
        v0_caps.as_deref(),
    );
    if opts.promisor_object_fetch {
        opts.mark_received_pack_promisor = true;
    } else {
        opts.mark_received_pack_promisor = opts.filter.is_some();
    }
    let wire_filter = wire_filter.as_deref();
    let local_shallow = crate::shallow::load_shallow_oids(local_git_dir)?;
    let mut transfer = crate::transfer::TransferStats::default();

    net_trace!(
        opts.network_trace,
        opts.diagnostics.as_ref(),
        "fetch_remote_object_wants: {} object(s) [+{:?} since begin]",
        wants.len(),
        t_begin.elapsed()
    );

    let (pack_path, shallow_update) = if let Some(caps) = v2_caps.as_ref() {
        validate_fetch_depth_server(opts, None, Some(caps))?;
        let deepen = V2DeepenArgs::from_opts(opts, &local_shallow);
        negotiate_pack_v2(
            local,
            conn,
            caps,
            &wants,
            &deepen,
            wire_filter,
            progress,
            cancel,
            V2FetchMode::ObjectWants,
        )?
    } else {
        negotiate_pack(
            local,
            conn,
            &wants,
            opts,
            &local_shallow,
            wire_filter,
            progress,
            cancel,
            true,
        )?
    };
    if let Some(path) = pack_path {
        let pack_config =
            crate::config::ConfigSet::load(&opts.environment, Some(local_git_dir), true)?;
        let index_threads = crate::parallel::index_pack_threads_for_ingest(
            opts.thread_policy,
            &pack_config,
            opts.index_pack_threads,
        )?;
        let ingested = crate::index_pack::ingest_received_pack_path_with_progress(
            path,
            local_odb,
            &crate::index_pack::IngestPackOptions {
                fix_thin: true,
                skip_post_index_verify: true,
                threads: index_threads,
                merged_config: Some(pack_config),
                environment: opts.environment.clone(),
                mark_promisor_pack: opts.mark_received_pack_promisor,
                trusted_index_rows: None,
            },
            Some(progress),
            Some(cancel),
        )?;
        transfer = crate::transfer::TransferStats::from_ingested_pack(&ingested);
    }

    if v2_caps.is_some() && !wants.is_empty() {
        conn.finish_send();
    }

    if !opts.dry_run {
        crate::shallow::apply_shallow_updates(
            local_git_dir,
            &shallow_update.shallow,
            &shallow_update.unshallow,
        )?;
    }

    Ok(FetchOutcome {
        updates: Vec::new(),
        default_branch: None,
        new_shallow: crate::shallow::dedupe_shallow_list(shallow_update.shallow),
        new_unshallow: shallow_update.unshallow,
        transfer,
        filter_sent,
        promisor: None,
    })
}

/// Committer identity for fetch/clone reflog entries.
pub fn fetch_operation_identity(
    env: &crate::environment::Environment,
    git_dir: &Path,
) -> Result<crate::ident::Signature> {
    use crate::ident_config::ident_default_name;
    let cfg = crate::config::ConfigSet::load(env, Some(git_dir), true)?;
    let name = ident_default_name(&cfg, env);
    let email = cfg
        .get("user.email")
        .filter(|e| !e.trim().is_empty())
        .unwrap_or_else(|| "unknown".to_owned());
    let now = time::OffsetDateTime::now_utc();
    crate::ident::Signature::new(name, email, now).map_err(crate::error::Error::InvalidSignature)
}

/// Ensure `refs/remotes/<remote>/HEAD` exists in `local`'s ref store when the remote default
/// branch is known and its tracking ref exists.
pub(crate) fn ensure_remote_tracking_head_symref(
    local: &Repository,
    remote: &str,
    default_branch: Option<&str>,
    dry_run: bool,
) -> Result<()> {
    if dry_run {
        return Ok(());
    }
    let Some(branch) = default_branch else {
        return Ok(());
    };
    let head_ref = format!("refs/remotes/{remote}/HEAD");
    if crate::refs::resolve_ref_in(local, &head_ref).is_ok() {
        return Ok(());
    }
    let target = format!("refs/remotes/{remote}/{branch}");
    if crate::refs::resolve_ref_in(local, &target).is_err() {
        return Ok(());
    }
    crate::refs::write_symbolic_ref_on_store(local.refs(), local.git_dir(), &head_ref, &target)
}

/// Clone-only ref layout after the first fetch into `local`: pack the imported refs (files
/// repositories only) and point `refs/remotes/<remote>/HEAD` at the default branch, with the
/// optional clone reflog entry.
pub(crate) fn finish_initial_remote_fetch_layout(
    local: &Repository,
    opts: &FetchOptions,
    default_branch: Option<&str>,
) -> Result<()> {
    if opts.dry_run || !opts.initial_remote_fetch {
        return Ok(());
    }
    let git_dir = local.git_dir();
    if local.refs_on_disk() {
        crate::refs::pack_imported_refs_for_clone(git_dir, opts.remote_name.as_deref())?;
    }
    let Some(remote) = opts.remote_name.as_deref() else {
        return Ok(());
    };
    let Some(branch) = default_branch else {
        return Ok(());
    };
    let target = format!("refs/remotes/{remote}/{branch}");
    let head_ref = format!("refs/remotes/{remote}/HEAD");
    let Ok(tip) = crate::refs::resolve_ref_in(local, &target) else {
        return Ok(());
    };
    crate::refs::write_symbolic_ref_on_store(local.refs(), git_dir, &head_ref, &target)?;
    let Some(ref log) = opts.clone_reflog else {
        return Ok(());
    };
    let null = ObjectId::null(local.odb().hash_algo());
    if local.refs_on_disk() {
        return crate::refs::append_reflog_with_signature(
            git_dir,
            &head_ref,
            &null,
            &tip,
            &log.identity,
            &log.message,
            true,
        );
    }
    log.identity
        .validate()
        .map_err(crate::error::Error::InvalidSignature)?;
    let identity = crate::refs::store::format_reflog_identity(
        &log.identity.reflog_name_email(),
        log.identity.time,
    );
    let config = local.config()?;
    crate::refs::append_reflog_on_store(
        local.refs(),
        &head_ref,
        &null,
        &tip,
        &identity,
        &log.message,
        true,
        &config,
    )
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
/// out of the default `want` list so an unreachable tag does not drag its target
/// into the pack (which would make it look reachable and survive the prune).
/// Annotated tags whose peeled commit is already local are still wanted via
/// [`following_tag_wants_with_local_peel`].
///
/// Under `Following`, tags named in `already_local` (see [`tags_already_local`])
/// are skipped: the local repository already has them at the advertised value.
#[allow(clippy::too_many_arguments)]
pub(crate) fn add_wire_tags(
    mode: crate::transfer::TagMode,
    remote_refs: &[(String, ObjectId)],
    advertised_peel: &std::collections::HashMap<String, ObjectId>,
    negatives: &[RefspecItem],
    already_local: &HashSet<String>,
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
        if mode == crate::transfer::TagMode::Following && already_local.contains(name) {
            continue;
        }
        seen_remote_ref.insert(name.clone());
        // Only tag oids newly added here belong in `following_only`. A lightweight
        // tag at the same commit as an already-matched branch tip must not suppress
        // the branch's `want` when building the pack request.
        if matched_oids.insert(*oid) && mode == crate::transfer::TagMode::Following {
            following_only.insert(*oid);
        }
        matched.push(crate::transfer::MatchedRef {
            remote_ref: name.clone(),
            local_ref: Some(name.clone()),
            oid: *oid,
            force: false,
            is_tag: true,
            auto_followed: mode == crate::transfer::TagMode::Following,
            dest_is_tag: true,
            advertised_peel: advertised_peel.get(name).copied(),
            fetch_head: crate::fetch_request::FetchHeadPolicy::ForMerge,
        });
    }
    following_only
}

/// Under `TagMode::Following`, tag OIDs in `following_only` that should still
/// be `want`ed: annotated tags pointing at a commit already present locally.
pub(crate) fn following_tag_wants_with_local_peel(
    matched: &[crate::transfer::MatchedRef],
    following_only: &HashSet<ObjectId>,
    local_odb: &crate::odb::Odb,
) -> Result<HashSet<ObjectId>> {
    let mut wants = HashSet::new();
    for m in matched {
        if !m.is_tag || !following_only.contains(&m.oid) {
            continue;
        }
        let Some(peel) = m.advertised_peel else {
            continue;
        };
        if local_odb.contains(&peel)? && !local_odb.contains(&m.oid)? {
            wants.insert(m.oid);
        }
    }
    Ok(wants)
}

/// Commit OIDs reachable within an absolute shallow `depth` from each root,
/// walking all parents (upload-pack `deepen` shape), stopping at
/// `shallow_boundaries`.
pub(crate) fn shallow_fetched_commit_oids(
    odb: &crate::odb::Odb,
    roots: &[ObjectId],
    depth: u32,
    shallow_boundaries: &HashSet<ObjectId>,
) -> Result<HashSet<ObjectId>> {
    use crate::objects::{parse_commit, ObjectKind};
    use std::collections::{HashMap, VecDeque};

    if depth == 0 {
        return Ok(HashSet::new());
    }
    let max_depth = usize::try_from(depth).unwrap_or(usize::MAX);
    let mut best_depth: HashMap<ObjectId, usize> = HashMap::new();
    let mut q: VecDeque<(ObjectId, usize)> = VecDeque::new();
    for &root in roots {
        best_depth.insert(root, 1);
        q.push_back((root, 1));
    }
    let mut out = HashSet::new();
    while let Some((oid, d)) = q.pop_front() {
        if d > max_depth {
            continue;
        }
        if best_depth.get(&oid).copied() != Some(d) {
            continue;
        }
        out.insert(oid);
        if d == max_depth {
            continue;
        }
        if shallow_boundaries.contains(&oid) {
            continue;
        }
        if !odb.contains(&oid)? {
            continue;
        }
        let Ok(obj) = odb.read(&oid) else {
            continue;
        };
        if obj.kind != ObjectKind::Commit {
            continue;
        }
        let Ok(c) = parse_commit(&obj.data) else {
            continue;
        };
        for p in c.parents {
            let nd = d.saturating_add(1);
            if nd > max_depth {
                continue;
            }
            let prev = best_depth.get(&p).copied().unwrap_or(usize::MAX);
            if nd < prev {
                best_depth.insert(p, nd);
                q.push_back((p, nd));
            }
        }
    }
    Ok(out)
}

/// Commits whose peels count as "fetched" for shallow tag-following: branch tips
/// when there is no absolute depth, otherwise the depth-limited closure.
pub(crate) fn following_peel_commit_set_for_shallow(
    odb: &crate::odb::Odb,
    commit_tips: &HashSet<ObjectId>,
    depth: Option<u32>,
    shallow_boundaries: &HashSet<ObjectId>,
) -> Result<HashSet<ObjectId>> {
    let roots: Vec<ObjectId> = commit_tips.iter().copied().collect();
    match depth.filter(|d| *d > 0) {
        Some(d) => shallow_fetched_commit_oids(odb, &roots, d, shallow_boundaries),
        None => Ok(commit_tips.clone()),
    }
}

/// On a shallow fetch, annotated tags whose peel is a branch tip being fetched
/// must be copied even when the peel is not local yet (matches upload-pack
/// include-tag under depth).
pub(crate) fn following_tag_wants_peel_among_fetched_commits(
    matched: &[crate::transfer::MatchedRef],
    following_only: &HashSet<ObjectId>,
    fetch_commit_oids: &HashSet<ObjectId>,
) -> HashSet<ObjectId> {
    let mut wants = HashSet::new();
    for m in matched {
        if !m.is_tag || !following_only.contains(&m.oid) {
            continue;
        }
        let Some(peel) = m.advertised_peel else {
            continue;
        };
        if peel != m.oid && fetch_commit_oids.contains(&peel) {
            wants.insert(m.oid);
        }
    }
    wants
}

/// Drop provisional `Following` tags that do not point into the fetched
/// history, i.e. whose peeled target is not reachable from the matched non-tag
/// refs. Matches `git fetch`'s default tag-following.
///
/// `pack_oids` holds the objects this fetch delivered. Every caller only
/// receives objects reachable from what it wanted (wire fetch), or has already
/// limited the provisional tags to reachable ones (local and bundle fetch), so
/// a target inside `pack_oids` is kept without reading any object. Commit
/// targets that were already local are checked with a commit walk, which stops
/// as soon as all of them are found. Tree and blob targets that were already
/// local are kept, as Git does: proving their reachability would mean walking
/// every tree of the local history. Targets that are neither delivered nor
/// local cannot be in the fetched history and are dropped. An annotated tag is
/// kept only when its tag object is delivered or already local. Objects that
/// are not stored locally are never read, so a partial clone does not
/// lazy-fetch anything while following tags.
///
/// Only rows marked [`auto_followed`](crate::transfer::MatchedRef::auto_followed)
/// are candidates: a tag a refspec selected is kept like any other ref the
/// caller asked for.
pub(crate) fn retain_following_tags(
    local_odb: &crate::odb::Odb,
    matched: &mut Vec<crate::transfer::MatchedRef>,
    pack_oids: &HashSet<ObjectId>,
    shallow_boundaries: &HashSet<ObjectId>,
) -> Result<()> {
    let followed = |m: &crate::transfer::MatchedRef| m.is_tag && m.auto_followed;
    if !matched.iter().any(followed) {
        return Ok(());
    }

    let roots: Vec<ObjectId> = matched
        .iter()
        .filter(|m| !m.is_tag)
        .map(|m| m.oid)
        .collect();
    let peeled_by_tag: std::collections::HashMap<ObjectId, ObjectId> = matched
        .iter()
        .filter(|m| followed(m))
        .map(|m| {
            let peeled = m.advertised_peel.unwrap_or_else(|| {
                local_odb
                    .peel_stored_tag_target(m.oid)
                    .ok()
                    .flatten()
                    .unwrap_or(m.oid)
            });
            (m.oid, peeled)
        })
        .collect();
    let mut local_commits: HashSet<ObjectId> = HashSet::new();
    let mut local_trees_and_blobs: HashSet<ObjectId> = HashSet::new();
    for &oid in peeled_by_tag.values() {
        // `contains` never lazy-fetches; reading a missing object from a
        // partial clone's ODB would pull it from the promisor remote.
        if pack_oids.contains(&oid) || !local_odb.contains(&oid)? {
            continue;
        }
        match local_odb.read_info(&oid)?.kind {
            crate::objects::ObjectKind::Tree | crate::objects::ObjectKind::Blob => {
                local_trees_and_blobs.insert(oid);
            }
            crate::objects::ObjectKind::Commit => {
                local_commits.insert(oid);
            }
            // A tag here could not be peeled from local objects: its chain is broken.
            crate::objects::ObjectKind::Tag => {}
        }
    }
    let local_reach = reachable_commits(local_odb, &roots, &local_commits, shallow_boundaries)?;
    let mut keep = Vec::with_capacity(matched.len());
    for m in matched.iter() {
        if !followed(m) {
            keep.push(true);
            continue;
        }
        let peeled = peeled_by_tag.get(&m.oid).copied().unwrap_or(m.oid);
        let reachable = pack_oids.contains(&peeled)
            || local_trees_and_blobs.contains(&peeled)
            || local_reach.contains(&peeled);
        // Lightweight tags store the peeled commit directly; annotated tags need their
        // tag object (new in this fetch or already local).
        keep.push(
            reachable
                && (m.oid == peeled || pack_oids.contains(&m.oid) || local_odb.contains(&m.oid)?),
        );
    }
    let mut keep = keep.into_iter();
    matched.retain(|_| keep.next().unwrap_or(false));
    Ok(())
}

/// Names of the remote `refs/tags/*` entries in `remote_refs` that the local
/// repository already stores under the same name at the same object id.
///
/// Tag-following never needs to consider these: Git's auto-follow skips tags
/// that already exist locally, and there is nothing to fetch or write for them.
/// Excluding them up front keeps the reachability walk off the whole history
/// when the client already has every advertised tag (the common incremental
/// fetch).
///
/// # Parameters
///
/// * `local_refs` — the fetching repository's existing ref store.
/// * `remote_refs` — advertised `(name, oid)` pairs (peeled `^{}` entries excluded).
///
/// Unreadable local refs count as absent, so they still go through the regular
/// tag-following path. Resolution retains the backend's loose/symbolic ref rules
/// and cache freshness checks without reopening the store for each tag.
pub(crate) fn tags_already_local(
    local_refs: &dyn crate::refs::store::RefStore,
    remote_refs: &[(String, ObjectId)],
) -> HashSet<String> {
    remote_refs
        .iter()
        .filter(|(name, oid)| {
            name.starts_with("refs/tags/") && local_refs.resolve(name).ok() == Some(*oid)
        })
        .map(|(name, _)| name.clone())
        .collect()
}

/// Commit-level reachability from `roots`: the set of commit OIDs reachable by
/// following commit parents (peeling annotated tags to their target). Unlike
/// [`reachable_closure`] it never descends into trees or blobs, so it stays cheap
/// across thousands of heads. Sufficient for tag-following retention, whose only
/// question is whether a tag's target commit is reachable from the fetched heads.
///
/// The walk stops as soon as every oid in `targets` has been reached, so the
/// returned set is only guaranteed complete for membership tests of `targets`
/// (and is the full closure whenever some target is unreachable). An empty
/// `targets` set returns immediately with an empty result.
pub(crate) fn reachable_commits(
    odb: &crate::odb::Odb,
    roots: &[ObjectId],
    targets: &HashSet<ObjectId>,
    shallow_boundaries: &HashSet<ObjectId>,
) -> Result<HashSet<ObjectId>> {
    use crate::objects::{parse_commit, parse_tag, ObjectKind};

    let mut seen: HashSet<ObjectId> = HashSet::new();
    let mut remaining: HashSet<ObjectId> = targets.clone();
    if remaining.is_empty() {
        return Ok(seen);
    }

    // Read commit parents from the commit-graph file when present — an mmap-style
    // lookup that avoids decompressing/parsing every commit object (the dominant
    // cost on large repos). Commits absent from the graph (recently fetched) and
    // octopus merges fall back to the object read below.
    let graph = crate::commit_graph_file::CommitGraphChain::load(odb.objects_dir());

    let mut stack: Vec<ObjectId> = roots.to_vec();
    while let Some(oid) = stack.pop() {
        if remaining.is_empty() {
            break;
        }
        if seen.contains(&oid) {
            continue;
        }
        if shallow_boundaries.contains(&oid) {
            if odb.contains(&oid)? {
                seen.insert(oid);
                remaining.remove(&oid);
            }
            continue;
        }
        if let Some(graph) = graph.as_ref() {
            if let Some((parents, _time)) = graph.graph_commit(&oid) {
                seen.insert(oid);
                remaining.remove(&oid);
                stack.extend(parents);
                continue;
            }
        }
        let Ok(obj) = odb.read(&oid) else {
            continue;
        };
        seen.insert(oid);
        remaining.remove(&oid);
        match obj.kind {
            ObjectKind::Commit => {
                if let Ok(c) = parse_commit(&obj.data) {
                    for p in c.parents {
                        stack.push(p);
                    }
                }
            }
            ObjectKind::Tag => {
                if let Ok(t) = parse_tag(&obj.data) {
                    stack.push(t.object);
                }
            }
            // Tree/blob reachability is irrelevant to tag-following; don't descend.
            ObjectKind::Tree | ObjectKind::Blob => {}
        }
    }
    Ok(seen)
}

#[cfg(test)]
mod fetch_advertised_ref_tests {
    use super::*;
    use crate::objects::ObjectId;
    use std::io::Cursor;

    fn linear_history(len: usize) -> (tempfile::TempDir, Vec<ObjectId>) {
        use crate::objects::ObjectKind;
        let dir = tempfile::tempdir().unwrap();
        let repo = crate::repo::init_repository(
            dir.path(),
            true,
            "main",
            None,
            crate::RefStorageFormat::Files,
        )
        .unwrap();
        let odb = repo.odb();
        let tree = odb.write(ObjectKind::Tree, b"").unwrap();
        let mut commits: Vec<ObjectId> = Vec::new();
        for i in 0..len {
            let mut body = format!("tree {tree}\n");
            if let Some(parent) = commits.last() {
                body.push_str(&format!("parent {parent}\n"));
            }
            let time = 1_000_000 + i;
            body.push_str(&format!(
                "author T <t@e.com> {time} +0000\ncommitter T <t@e.com> {time} +0000\n\nc{i}\n"
            ));
            commits.push(odb.write(ObjectKind::Commit, body.as_bytes()).unwrap());
        }
        let tip = commits.last().copied().unwrap();
        crate::refs::write_ref(repo.git_dir(), "refs/heads/main", &tip).unwrap();
        (dir, commits)
    }

    #[test]
    fn v2_local_haves_are_produced_only_as_far_as_asked() {
        let (dir, commits) = linear_history(400);
        let unrelated_want = ObjectId::from_bytes(&[9u8; 20]).unwrap();
        let local = Repository::open(dir.path(), &RepositoryOptions::empty()).unwrap();

        let mut drained = v2_local_haves(&local, &[unrelated_want]).unwrap();
        let all = drained.all().unwrap().to_vec();
        assert!(drained.exhausted);
        assert_eq!(all[0], *commits.last().unwrap());
        assert!(
            all.len() > 4,
            "skipping still offers several haves: {}",
            all.len()
        );

        let mut lazy = v2_local_haves(&local, &[unrelated_want]).unwrap();
        let first = lazy.first(4).unwrap().to_vec();
        assert_eq!(first.as_slice(), &all[..4]);
        assert_eq!(lazy.taken.len(), 4, "only the requested haves are walked");
        assert!(!lazy.exhausted);
        assert_eq!(lazy.all().unwrap(), all.as_slice());
    }

    #[test]
    fn v2_local_haves_first_stops_at_a_short_history() {
        let (dir, _) = linear_history(3);
        let want = ObjectId::from_bytes(&[9u8; 20]).unwrap();
        let local = Repository::open(dir.path(), &RepositoryOptions::empty()).unwrap();
        let all = v2_local_haves(&local, &[want])
            .unwrap()
            .all()
            .unwrap()
            .to_vec();
        let mut haves = v2_local_haves(&local, &[want]).unwrap();
        assert_eq!(haves.first(INITIAL_FLUSH).unwrap(), all.as_slice());
        assert!(haves.exhausted);
    }

    #[test]
    fn v2_ls_refs_prefixes_follow_refspecs_and_tag_mode() {
        use crate::transfer::TagMode;

        let heads = vec!["+refs/heads/*:refs/remotes/origin/*".to_owned()];
        assert_eq!(v2_ls_refs_prefixes(&heads, TagMode::None), ["refs/heads/"]);
        assert_eq!(
            v2_ls_refs_prefixes(&heads, TagMode::Following),
            ["refs/heads/", "refs/tags/"]
        );
        let short = vec!["main".to_owned()];
        assert_eq!(
            v2_ls_refs_prefixes(&short, TagMode::None),
            [
                "main",
                "refs/main",
                "refs/tags/main",
                "refs/heads/main",
                "refs/remotes/main",
                "refs/remotes/main/HEAD",
            ]
        );
        let oid = vec![format!("{}:refs/heads/x", "a".repeat(40))];
        assert_eq!(
            v2_ls_refs_prefixes(&oid, TagMode::None),
            fetch_ref_list_prefixes(&oid)
        );
    }

    #[test]
    fn fetch_ref_list_prefixes_includes_refs_dwim_for_pull() {
        let specs = vec!["pull/123/head:pr-123".to_owned()];
        let prefixes = fetch_ref_list_prefixes(&specs);
        assert!(prefixes.iter().any(|p| p == "refs/pull/123/head"));
        assert!(prefixes.iter().any(|p| p == "refs/heads/"));
    }

    #[test]
    fn build_v2_ls_refs_request_includes_unborn_when_advertised() {
        use crate::odb::Odb;
        use crate::transfer::TagMode;

        let caps = vec!["ls-refs=unborn".to_owned()];
        let odb = Odb::new(std::path::Path::new("/dev/null"));
        let req = build_v2_ls_refs_request(&caps, &odb, TagMode::None, &[], None).unwrap();
        let text = String::from_utf8_lossy(&req);
        assert!(
            text.contains("unborn"),
            "request must ask for unborn HEAD:\n{text}"
        );
        assert!(!text.contains("promisor-remote"), "{text}");
    }

    #[test]
    fn build_v2_ls_refs_request_carries_promisor_reply_before_args() {
        use crate::odb::Odb;
        use crate::transfer::TagMode;

        let caps = vec!["agent=git/2.50".to_owned(), "object-format=sha1".to_owned()];
        let odb = Odb::new(std::path::Path::new("/dev/null"));
        let req = build_v2_ls_refs_request(&caps, &odb, TagMode::None, &[], Some("lop")).unwrap();
        let text = String::from_utf8_lossy(&req);
        let reply = text.find("promisor-remote=lop\n").expect("reply line");
        let delim = text.find("0001").expect("delim");
        assert!(reply < delim, "{text}");
    }

    #[test]
    fn v2_acknowledgments_collect_acked_commits_and_ready() {
        let a = ObjectId::from_hex("aabbccddeeff00112233445566778899aabbccdd").unwrap();
        let b = ObjectId::from_hex("00112233445566778899aabbccddeeff00112233").unwrap();
        let mut body = Vec::new();
        crate::pkt_line::write_line(&mut body, "acknowledgments").unwrap();
        crate::pkt_line::write_line(&mut body, &format!("ACK {a}")).unwrap();
        crate::pkt_line::write_line(&mut body, &format!("ACK {b}")).unwrap();
        crate::pkt_line::write_line(&mut body, "ready").unwrap();
        crate::pkt_line::write_delim(&mut body).unwrap();

        let round = read_v2_acknowledgments(&mut Cursor::new(body), false)
            .unwrap()
            .unwrap();
        assert!(round.ready);
        assert_eq!(round.common, vec![a, b]);
    }

    #[test]
    fn v2_acknowledgments_nak_has_no_common() {
        let mut body = Vec::new();
        crate::pkt_line::write_line(&mut body, "acknowledgments").unwrap();
        crate::pkt_line::write_line(&mut body, "NAK").unwrap();
        crate::pkt_line::write_flush(&mut body).unwrap();

        let round = read_v2_acknowledgments(&mut Cursor::new(body), false)
            .unwrap()
            .unwrap();
        assert!(!round.ready);
        assert!(round.common.is_empty());
    }

    #[test]
    fn v2_acknowledgments_reject_malformed_ack() {
        let mut body = Vec::new();
        crate::pkt_line::write_line(&mut body, "acknowledgments").unwrap();
        crate::pkt_line::write_line(&mut body, "ACK not-an-oid").unwrap();
        crate::pkt_line::write_flush(&mut body).unwrap();

        assert!(read_v2_acknowledgments(&mut Cursor::new(body), false).is_err());
    }

    #[test]
    fn v2_flush_schedule_doubles_then_grows_by_a_tenth() {
        let mut sizes = vec![INITIAL_FLUSH];
        while sizes.len() < 6 {
            let last = *sizes.last().unwrap();
            sizes.push(next_v2_flush_count(last));
        }
        assert_eq!(sizes, vec![16, 32, 64, 128, 256, 512]);
        assert_eq!(
            next_v2_flush_count(LARGE_FLUSH),
            LARGE_FLUSH + LARGE_FLUSH / 10
        );
    }

    #[test]
    fn parse_v2_ls_refs_unborn_head_symref_line() {
        let mut body = Vec::new();
        crate::pkt_line::write_line(&mut body, "unborn HEAD symref-target:refs/heads/trunk")
            .unwrap();
        crate::pkt_line::write_flush(&mut body).unwrap();

        let (_, head, _, _) = parse_v2_ls_refs_response(&mut Cursor::new(body)).unwrap();
        assert_eq!(head.as_deref(), Some("refs/heads/trunk"));
    }

    #[test]
    fn parse_v2_ls_refs_drops_traversal_ref() {
        let oid = ObjectId::from_hex("aabbccddeeff00112233445566778899aabbccdd").unwrap();
        let line = format!("{oid} refs/heads/../../../config");
        let mut body = Vec::new();
        crate::pkt_line::write_line(&mut body, &line).unwrap();
        crate::pkt_line::write_flush(&mut body).unwrap();

        let (refs, _, _, _) = parse_v2_ls_refs_response(&mut Cursor::new(body)).unwrap();
        assert!(refs.is_empty());
    }

    #[test]
    fn refspec_maps_traversal_to_non_storable_local_name() {
        use crate::refs::is_valid_storable_ref_name;
        use crate::refspec::parse_fetch_refspec;
        use crate::transfer::match_positive;

        let spec = parse_fetch_refspec("+refs/heads/*:refs/remotes/origin/*").unwrap();
        let positive = vec![spec];
        let local = match_positive("refs/heads/../../../config", &positive)
            .flatten()
            .expect("wildcard matches malicious remote name");
        assert!(!is_valid_storable_ref_name(&local));
    }

    #[test]
    fn add_wire_tags_following_skips_following_only_when_oid_already_matched() {
        use crate::transfer::TagMode;
        use std::collections::{HashMap, HashSet};

        let shared = ObjectId::from_hex("4eda3c62a0f0a0a0a0a0a0a0a0a0a0a0a0a0a0a0").unwrap();
        let unreachable_tag =
            ObjectId::from_hex("1111111111111111111111111111111111111111").unwrap();
        let remote_refs = vec![
            ("refs/heads/rel".to_owned(), shared),
            ("refs/tags/light".to_owned(), shared),
            ("refs/tags/orphan".to_owned(), unreachable_tag),
        ];
        let advertised_peel = HashMap::new();
        let negatives: Vec<crate::refspec::RefspecItem> = Vec::new();
        let mut matched = Vec::new();
        let mut matched_oids = HashSet::from([shared]);
        let mut seen = HashSet::from(["refs/heads/rel".to_owned()]);
        matched.push(crate::transfer::MatchedRef {
            remote_ref: "refs/heads/rel".to_owned(),
            local_ref: Some("refs/remotes/origin/rel".to_owned()),
            oid: shared,
            force: false,
            is_tag: false,
            auto_followed: false,
            dest_is_tag: false,
            advertised_peel: None,
            fetch_head: crate::fetch_request::FetchHeadPolicy::ForMerge,
        });

        let following_only = add_wire_tags(
            TagMode::Following,
            &remote_refs,
            &advertised_peel,
            &negatives,
            &HashSet::new(),
            &mut matched,
            &mut matched_oids,
            &mut seen,
        );

        assert!(
            !following_only.contains(&shared),
            "shared branch/tag tip must stay wanted"
        );
        assert!(
            following_only.contains(&unreachable_tag),
            "tag-only oid stays following-only (no upfront want)"
        );
        assert_eq!(matched.iter().filter(|m| m.is_tag).count(), 2);
    }

    #[test]
    fn following_tag_wants_includes_annotated_tag_when_peel_is_local() {
        use std::collections::HashSet;

        let tag_obj = ObjectId::from_hex("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb").unwrap();
        let tmp = tempfile::tempdir().expect("tempdir");
        let git_dir = tmp.path().join(".git");
        std::fs::create_dir_all(git_dir.join("objects")).expect("objects");
        let odb = crate::odb::Odb::new(git_dir.join("objects").as_path());
        // Seed only the peeled commit (tag object absent).
        let commit_bytes = b"tree 0000000000000000000000000000000000000000\nauthor T <t@example.com> 0 +0000\ncommitter T <t@example.com> 0 +0000\n\nx\n";
        let commit = odb
            .write_loose_materialize(crate::objects::ObjectKind::Commit, commit_bytes)
            .expect("write commit");

        let following_only = HashSet::from([tag_obj]);
        let matched = vec![crate::transfer::MatchedRef {
            remote_ref: "refs/tags/v".to_owned(),
            local_ref: Some("refs/tags/v".to_owned()),
            oid: tag_obj,
            force: false,
            is_tag: true,
            auto_followed: true,
            dest_is_tag: true,
            advertised_peel: Some(commit),
            fetch_head: crate::fetch_request::FetchHeadPolicy::ForMerge,
        }];
        let wants = following_tag_wants_with_local_peel(&matched, &following_only, &odb).unwrap();
        assert!(wants.contains(&tag_obj));
    }

    #[test]
    fn following_tag_wants_peel_among_fetched_commits_before_peel_is_local() {
        use std::collections::HashSet;

        let tag_obj = ObjectId::from_hex("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb").unwrap();
        let tip = ObjectId::from_hex("cccccccccccccccccccccccccccccccccccccccc").unwrap();
        let following_only = HashSet::from([tag_obj]);
        let fetch_commits = HashSet::from([tip]);
        let matched = vec![crate::transfer::MatchedRef {
            remote_ref: "refs/tags/v".to_owned(),
            local_ref: Some("refs/tags/v".to_owned()),
            oid: tag_obj,
            force: false,
            is_tag: true,
            auto_followed: true,
            dest_is_tag: true,
            advertised_peel: Some(tip),
            fetch_head: crate::fetch_request::FetchHeadPolicy::ForMerge,
        }];
        let wants = following_tag_wants_peel_among_fetched_commits(
            &matched,
            &following_only,
            &fetch_commits,
        );
        assert!(wants.contains(&tag_obj));
    }

    #[test]
    fn shallow_peel_set_includes_parent_at_depth_two_not_tip_only() {
        use std::collections::HashSet;

        let tmp = tempfile::tempdir().expect("tempdir");
        let objects = tmp.path().join("objects");
        std::fs::create_dir_all(&objects).expect("objects");
        let odb = crate::odb::Odb::new(objects.as_path());
        let chain = write_commit_chain(&odb, 3);
        let tip = *chain.last().expect("tip");
        let parent = chain[1];
        let tips = HashSet::from([tip]);
        let tips_only =
            following_peel_commit_set_for_shallow(&odb, &tips, Some(1), &HashSet::new()).unwrap();
        assert!(!tips_only.contains(&parent));
        let depth_two =
            following_peel_commit_set_for_shallow(&odb, &tips, Some(2), &HashSet::new()).unwrap();
        assert!(depth_two.contains(&parent));
    }

    /// Writes a linear chain of `n` commits (oldest first) into `odb`.
    fn write_commit_chain(odb: &crate::odb::Odb, n: usize) -> Vec<ObjectId> {
        let mut chain: Vec<ObjectId> = Vec::new();
        for i in 0..n {
            let parent = chain
                .last()
                .map(|p| format!("parent {}\n", p.to_hex()))
                .unwrap_or_default();
            let body = format!(
                "tree 4b825dc642cb6eb9a060e54bf8d69288fbee4904\n{parent}\
                 author T <t@example.com> {i} +0000\ncommitter T <t@example.com> {i} +0000\n\nc{i}\n"
            );
            let oid = odb
                .write_loose_materialize(crate::objects::ObjectKind::Commit, body.as_bytes())
                .expect("write commit");
            chain.push(oid);
        }
        chain
    }

    fn matched_ref(
        name: &str,
        oid: ObjectId,
        peel: Option<ObjectId>,
    ) -> crate::transfer::MatchedRef {
        let is_tag = name.starts_with("refs/tags/");
        crate::transfer::MatchedRef {
            remote_ref: name.to_owned(),
            local_ref: Some(name.to_owned()),
            oid,
            force: false,
            is_tag,
            auto_followed: is_tag,
            dest_is_tag: is_tag,
            advertised_peel: peel,
            fetch_head: crate::fetch_request::FetchHeadPolicy::ForMerge,
        }
    }

    fn kept_tags(matched: &[crate::transfer::MatchedRef]) -> Vec<&str> {
        let mut names: Vec<&str> = matched
            .iter()
            .filter(|m| m.is_tag)
            .map(|m| m.remote_ref.as_str())
            .collect();
        names.sort_unstable();
        names
    }

    #[test]
    fn retain_following_tags_keeps_delivered_targets_without_reading_objects() {
        use std::collections::HashSet;

        // The object database is empty: deciding on delivered targets must not
        // read (inflate) any commit, tree or blob.
        let tmp = tempfile::tempdir().expect("tempdir");
        let objects = tmp.path().join("objects");
        std::fs::create_dir_all(&objects).expect("objects");
        let odb = crate::odb::Odb::new(&objects);
        let oid = |c: char| ObjectId::from_hex(&c.to_string().repeat(40)).expect("oid");
        let (tip, tree, blob, tag_obj, missing_tag_obj, absent) =
            (oid('1'), oid('2'), oid('3'), oid('4'), oid('5'), oid('6'));
        let pack_oids = HashSet::from([tip, tree, blob, tag_obj]);
        let mut matched = vec![
            matched_ref("refs/heads/main", tip, None),
            matched_ref("refs/tags/light", tip, None),
            matched_ref("refs/tags/blob", blob, None),
            matched_ref("refs/tags/annotated-tree", tag_obj, Some(tree)),
            matched_ref("refs/tags/no-tag-object", missing_tag_obj, Some(tip)),
            matched_ref("refs/tags/undelivered", absent, None),
        ];

        retain_following_tags(&odb, &mut matched, &pack_oids, &HashSet::new())
            .expect("retain following tags");

        assert_eq!(
            kept_tags(&matched),
            [
                "refs/tags/annotated-tree",
                "refs/tags/blob",
                "refs/tags/light"
            ]
        );
        assert!(matched.iter().any(|m| m.remote_ref == "refs/heads/main"));
    }

    #[test]
    fn retain_following_tags_keeps_refspec_selected_tags() {
        use std::collections::HashSet;

        let tmp = tempfile::tempdir().expect("tempdir");
        let objects = tmp.path().join("objects");
        std::fs::create_dir_all(&objects).expect("objects");
        let odb = crate::odb::Odb::new(&objects);
        // The peeled commit is local, the annotated tag object is not, no
        // branch is fetched and the pack is empty: a moved release tag.
        let peel = write_commit_chain(&odb, 1)[0];
        let tag_obj = ObjectId::from_hex(&"9".repeat(40)).expect("oid");
        let followed = matched_ref("refs/tags/followed", tag_obj, Some(peel));
        let mut selected = matched_ref("refs/tags/selected", tag_obj, Some(peel));
        selected.auto_followed = false;
        let mut matched = vec![followed, selected];

        retain_following_tags(&odb, &mut matched, &HashSet::new(), &HashSet::new())
            .expect("retain following tags");

        assert_eq!(kept_tags(&matched), ["refs/tags/selected"]);
    }

    #[test]
    fn retain_following_tags_walks_commits_only_for_targets_already_local() {
        use std::collections::HashSet;

        let tmp = tempfile::tempdir().expect("tempdir");
        let objects = tmp.path().join("objects");
        std::fs::create_dir_all(&objects).expect("objects");
        let odb = crate::odb::Odb::new(&objects);
        let chain = write_commit_chain(&odb, 4);
        let tip = chain[3];
        // A second local history that the fetched tip does not reach.
        let unrelated = odb
            .write_loose_materialize(
                crate::objects::ObjectKind::Commit,
                b"tree 4b825dc642cb6eb9a060e54bf8d69288fbee4904\n\
                  author T <t@example.com> 9 +0000\ncommitter T <t@example.com> 9 +0000\n\nu\n",
            )
            .expect("write commit");
        let blob = odb
            .write_loose_materialize(crate::objects::ObjectKind::Blob, b"local blob\n")
            .expect("write blob");
        let mut matched = vec![
            matched_ref("refs/heads/main", tip, None),
            matched_ref("refs/tags/old", chain[0], None),
            matched_ref("refs/tags/unrelated", unrelated, None),
            matched_ref("refs/tags/local-blob", blob, None),
        ];

        retain_following_tags(&odb, &mut matched, &HashSet::new(), &HashSet::new())
            .expect("retain following tags");
        assert_eq!(
            kept_tags(&matched),
            ["refs/tags/local-blob", "refs/tags/old"]
        );

        // A shallow boundary stops the walk: history behind it is not fetched.
        let mut matched = vec![
            matched_ref("refs/heads/main", tip, None),
            matched_ref("refs/tags/at-boundary", chain[1], None),
            matched_ref("refs/tags/behind-boundary", chain[0], None),
        ];
        retain_following_tags(
            &odb,
            &mut matched,
            &HashSet::new(),
            &HashSet::from([chain[1]]),
        )
        .expect("retain following tags");
        assert_eq!(kept_tags(&matched), ["refs/tags/at-boundary"]);
    }

    #[test]
    fn retain_following_tags_never_lazy_fetches_in_a_partial_clone() {
        use std::collections::HashSet;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        struct CountingFetcher(Arc<AtomicUsize>);
        impl crate::promisor_fetch::PromisorFetcher for CountingFetcher {
            fn fetch_missing(
                &self,
                _repo: &crate::repo::Repository,
                _oids: &[ObjectId],
            ) -> Result<()> {
                self.0.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
        }

        let tmp = tempfile::tempdir().expect("tempdir");
        let git_dir = tmp.path().join(".git");
        std::fs::create_dir_all(git_dir.join("objects/pack")).expect("objects");
        std::fs::create_dir_all(git_dir.join("refs/heads")).expect("refs");
        std::fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n").expect("HEAD");
        std::fs::write(
            git_dir.join("config"),
            "[core]\n\trepositoryformatversion = 1\n\tbare = true\n\
             [extensions]\n\tpartialclone = origin\n\
             [remote \"origin\"]\n\turl = /nonexistent\n\tpromisor = true\n",
        )
        .expect("config");
        let calls = Arc::new(AtomicUsize::new(0));
        let repo = crate::repo::Repository::open(
            &git_dir,
            &RepositoryOptions::empty()
                .with_promisor_fetcher(Some(Arc::new(CountingFetcher(Arc::clone(&calls))))),
        )
        .expect("open");
        let odb = repo.odb();
        let tip = write_commit_chain(odb, 1)[0];
        let oid = |c: char| ObjectId::from_hex(&c.to_string().repeat(40)).expect("oid");
        // Neither the promised blob nor the annotated tag object is stored locally.
        let (promised_blob, missing_tag_obj) = (oid('7'), oid('8'));
        let write_tag = |target: ObjectId, kind: &str| {
            let body = format!(
                "object {}\ntype {kind}\ntag t\ntagger T <t@example.com> 0 +0000\n\nt\n",
                target.to_hex()
            );
            odb.write_loose_materialize(crate::objects::ObjectKind::Tag, body.as_bytes())
                .expect("write tag")
        };
        // Stored tags whose final target, or an intermediate tag, is missing.
        let local_tag_on_promised_blob = write_tag(promised_blob, "blob");
        let local_tag_on_missing_tag = write_tag(missing_tag_obj, "tag");
        let mut matched = vec![
            matched_ref("refs/heads/main", tip, None),
            matched_ref("refs/tags/light", tip, None),
            matched_ref("refs/tags/promised-blob", promised_blob, None),
            matched_ref("refs/tags/unpeeled", missing_tag_obj, None),
            matched_ref(
                "refs/tags/local-on-promised",
                local_tag_on_promised_blob,
                None,
            ),
            matched_ref(
                "refs/tags/local-on-missing-tag",
                local_tag_on_missing_tag,
                None,
            ),
        ];

        retain_following_tags(odb, &mut matched, &HashSet::new(), &HashSet::new())
            .expect("retain following tags");

        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "tag following must not lazy-fetch missing objects"
        );
        assert_eq!(kept_tags(&matched), ["refs/tags/light"]);
    }

    #[test]
    fn reachable_commits_stops_once_every_target_is_found() {
        use std::collections::HashSet;

        let tmp = tempfile::tempdir().expect("tempdir");
        let objects = tmp.path().join("objects");
        std::fs::create_dir_all(&objects).expect("objects");
        let odb = crate::odb::Odb::new(&objects);
        let chain = write_commit_chain(&odb, 5);
        let tip = chain[4];
        let none = HashSet::new();

        let near = HashSet::from([chain[3]]);
        let seen = reachable_commits(&odb, &[tip], &near, &none).unwrap();
        assert!(seen.contains(&chain[3]), "reachable target is reported");
        assert!(
            !seen.contains(&chain[0]),
            "walk must stop before the root once every target is found"
        );

        let unreachable =
            HashSet::from([
                ObjectId::from_hex("1111111111111111111111111111111111111111").unwrap(),
            ]);
        let seen = reachable_commits(&odb, &[tip], &unreachable, &none).unwrap();
        assert_eq!(
            seen,
            chain.iter().copied().collect::<HashSet<_>>(),
            "an unreachable target forces the full closure"
        );

        assert!(
            reachable_commits(&odb, &[tip], &HashSet::new(), &none)
                .unwrap()
                .is_empty(),
            "no targets means no walk"
        );
    }

    #[test]
    fn tags_already_local_matches_name_and_oid_across_loose_and_packed() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let git_dir = tmp.path().join(".git");
        std::fs::create_dir_all(git_dir.join("objects")).expect("objects");
        std::fs::create_dir_all(git_dir.join("refs/tags")).expect("refs/tags");
        std::fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n").expect("HEAD");

        let a = ObjectId::from_hex("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa").unwrap();
        let b = ObjectId::from_hex("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb").unwrap();
        let c = ObjectId::from_hex("cccccccccccccccccccccccccccccccccccccccc").unwrap();
        std::fs::write(git_dir.join("refs/tags/loose"), format!("{}\n", a.to_hex()))
            .expect("loose tag");
        std::fs::write(
            crate::refs::store::paths::packed_refs_path(&git_dir),
            format!(
                "# pack-refs with: peeled fully-peeled sorted \n{} refs/tags/moved\n{} refs/tags/packed\n",
                a.to_hex(),
                b.to_hex()
            ),
        )
        .expect("packed-refs");

        let remote_refs = vec![
            ("refs/heads/main".to_owned(), a),
            ("refs/tags/loose".to_owned(), a),
            ("refs/tags/packed".to_owned(), b),
            ("refs/tags/moved".to_owned(), c),
            ("refs/tags/new".to_owned(), c),
        ];
        let store = crate::refs::store::open_ref_store(&git_dir).unwrap();
        let present = tags_already_local(store.as_ref(), &remote_refs);
        assert_eq!(
            present,
            HashSet::from(["refs/tags/loose".to_owned(), "refs/tags/packed".to_owned()]),
            "only tags stored locally at the advertised oid count as present"
        );
        assert!(tags_already_local(store.as_ref(), &remote_refs[..1]).is_empty());
    }

    #[test]
    fn add_wire_tags_following_skips_tags_already_local() {
        use crate::transfer::TagMode;
        use std::collections::HashMap;

        let tip = ObjectId::from_hex("2222222222222222222222222222222222222222").unwrap();
        let old_tag = ObjectId::from_hex("3333333333333333333333333333333333333333").unwrap();
        let new_tag = ObjectId::from_hex("4444444444444444444444444444444444444444").unwrap();
        let remote_refs = vec![
            ("refs/heads/main".to_owned(), tip),
            ("refs/tags/old".to_owned(), old_tag),
            ("refs/tags/new".to_owned(), new_tag),
        ];
        let already_local = HashSet::from(["refs/tags/old".to_owned()]);

        for (mode, expect) in [
            (TagMode::Following, vec!["refs/tags/new"]),
            (TagMode::All, vec!["refs/tags/old", "refs/tags/new"]),
        ] {
            let mut matched = Vec::new();
            let mut matched_oids = HashSet::new();
            let mut seen = HashSet::new();
            add_wire_tags(
                mode,
                &remote_refs,
                &HashMap::new(),
                &[],
                &already_local,
                &mut matched,
                &mut matched_oids,
                &mut seen,
            );
            let names: Vec<&str> = matched.iter().map(|m| m.remote_ref.as_str()).collect();
            assert_eq!(names, expect, "{mode:?}");
        }
    }
}

#[cfg(test)]
mod v2_negotiation_tests {
    use super::*;
    use crate::objects::ObjectKind;
    use crate::progress::NeverCancel;
    use crate::repo::{InitOptions, Repository};
    use std::io::Cursor;

    /// A stateless v2 upload-pack stand-in that ACKs the first `have` it ever
    /// sees, re-ACKs it whenever a request repeats it, and never says `ready`.
    struct AckingNeverReadyServer {
        caps: Vec<String>,
        pending: Vec<u8>,
        response: Cursor<Vec<u8>>,
        acked: Option<ObjectId>,
        requests: Vec<Vec<String>>,
    }

    impl AckingNeverReadyServer {
        fn answer(&mut self) -> std::io::Result<()> {
            let mut lines = Vec::new();
            let mut cursor = Cursor::new(std::mem::take(&mut self.pending));
            while let Some(pkt) = pkt_line::read_packet(&mut cursor)? {
                if let pkt_line::Packet::Data(line) = pkt {
                    lines.push(line.trim_end().to_owned());
                }
            }
            let haves: Vec<ObjectId> = lines
                .iter()
                .filter_map(|l| l.strip_prefix("have "))
                .filter_map(|h| ObjectId::from_hex(h).ok())
                .collect();
            let done = lines.iter().any(|l| l == "done");
            self.requests.push(lines);
            let mut out = Vec::new();
            if !done {
                let acked = *self.acked.get_or_insert(haves[0]);
                pkt_line::write_line(&mut out, "acknowledgments")?;
                if haves.contains(&acked) {
                    pkt_line::write_line(&mut out, &format!("ACK {acked}"))?;
                } else {
                    pkt_line::write_line(&mut out, "NAK")?;
                }
                pkt_line::write_flush(&mut out)?;
            }
            self.response = Cursor::new(out);
            Ok(())
        }
    }

    impl Write for AckingNeverReadyServer {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.pending.extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            if self.pending.is_empty() {
                return Ok(());
            }
            self.answer()
        }
    }

    impl Connection for AckingNeverReadyServer {
        fn reader(&mut self) -> &mut dyn Read {
            &mut self.response
        }
        fn writer(&mut self) -> &mut dyn Write {
            self
        }
        fn advertised_refs(&self) -> &[(String, ObjectId)] {
            &[]
        }
        fn capabilities(&self) -> &[String] {
            &self.caps
        }
        fn head_symref(&self) -> Option<&str> {
            None
        }
        fn protocol_version(&self) -> u8 {
            2
        }
        fn stateless_rpc(&self) -> bool {
            true
        }
    }

    #[test]
    fn replayed_acks_do_not_postpone_the_in_vain_cutoff() {
        const TIPS: usize = 2000;
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::init(tmp.path(), &InitOptions::default()).unwrap();
        let git_dir = repo.git_dir().to_path_buf();
        let tree = repo.odb().write(ObjectKind::Tree, &[]).unwrap();
        // Unrelated root commits: each one is a have of its own, so the
        // negotiator never runs dry before the cutoff.
        for i in 0..TIPS {
            let when = 1_600_000_000 + i as i64;
            let body = format!(
                "tree {tree}\nauthor T <t@example.com> {when} +0000\n\
                 committer T <t@example.com> {when} +0000\n\nroot {i}\n"
            );
            let oid = repo
                .odb()
                .write(ObjectKind::Commit, body.as_bytes())
                .unwrap();
            crate::refs::write_ref(&git_dir, &format!("refs/heads/b{i:04}"), &oid).unwrap();
        }

        let mut server = AckingNeverReadyServer {
            caps: vec!["fetch".to_owned()],
            pending: Vec::new(),
            response: Cursor::new(Vec::new()),
            acked: None,
            requests: Vec::new(),
        };
        let want = ObjectId::from_hex("1111111111111111111111111111111111111111").unwrap();
        // The scripted server sends no pack after `done`; only the requests matter.
        let _ = negotiate_pack_v2(
            &repo,
            &mut server,
            &[],
            &[want],
            &V2DeepenArgs::default(),
            None,
            &mut NoProgress,
            &NeverCancel,
            V2FetchMode::Refs {
                promisor_reply: None,
            },
        );

        let last = server.requests.last().unwrap();
        assert!(
            last.iter().any(|l| l == "done"),
            "negotiation must end with done"
        );
        let distinct: HashSet<&str> = server
            .requests
            .iter()
            .flatten()
            .filter_map(|l| l.strip_prefix("have "))
            .collect();
        // 16 haves before the first ACK, then rounds of 32, 64, 128 and 256
        // haves the server never acknowledges (it only re-ACKs the replayed
        // common commit): 480 vain haves, so the cutoff fires with that round.
        assert_eq!(distinct.len(), 16 + 32 + 64 + 128 + 256);
        assert!(distinct.len() < TIPS, "cutoff never fired");
    }
}
