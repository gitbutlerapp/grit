//! Serving fetches and clones (`upload-pack`).

use super::promised_objects::{
    missing_objects_policy, plan_pack, resolve_missing_wants, MissingObjects,
};
use super::shallow_upload::{compute_shallow_response, ShallowRequest, ShallowResponse};
use super::upload_pack_policy::{
    resolve_want_ref, validate_wants, validate_wants_filtered, UploadPackPolicy,
};
use super::{
    check_object_format, for_each_advertised_ref, head_info, parse_oid, read_packet, write_flush,
    write_line, AdvertisedRefView, DeepenNotError, MissingObjectReason, Packet, Peel,
    ProtocolVersion, Result, ServeError, ServeOptions,
};
use crate::config::ConfigSet;
use crate::objects::pack_objects::PackStats;
use crate::objects::{ObjectId, ObjectKind};
use crate::pack_object_select::{PackObjectSelection, TagCandidate};
use crate::pack_objects::{
    write_upload_pack_selection, write_upload_pack_stream_with_cancel, PackProgress,
};
use crate::pkt_line::SidebandChannel1Writer;
use crate::progress::{Cancel, CancelSignal, NeverCancel};
use crate::promisor_remote::{parse_accepted_reply, PromisorAdvertisement};
use crate::repo::Repository;
use crate::rev_list::ObjectFilter;
use crate::transfer::PackBuildOptions;
use std::collections::HashSet;
use std::io::{Read, Write};
use std::ops::ControlFlow;

/// Capabilities offered in a v0/v1 advertisement, before the per-repository ones.
const V0_CAPABILITIES: &[&str] = &[
    "multi_ack_detailed",
    "side-band-64k",
    "thin-pack",
    "no-progress",
    "include-tag",
    "ofs-delta",
    "shallow",
    "deepen",
    "deepen-since",
    "deepen-not",
];

/// Summary of one completed or partial upload-pack session.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct UploadOutcome {
    /// Bytes written to the client (advertisement, negotiation, sideband, pack).
    pub bytes_written: u64,
    /// Objects sent in the pack, when a pack was written.
    pub objects_sent: u32,
    /// Negotiation rounds answered (v0 flush rounds and v2 acknowledgment sections).
    pub negotiation_rounds: u32,
    /// Pack build counters when a pack was sent.
    pub pack: Option<PackStats>,
}

struct CountingWriter<'a> {
    inner: &'a mut dyn Write,
    bytes: u64,
}

impl Write for CountingWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.bytes += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

#[derive(Default)]
struct UploadSession {
    pack: Option<PackStats>,
    negotiation_rounds: u32,
}

impl UploadSession {
    fn into_outcome(self, bytes_written: u64) -> UploadOutcome {
        UploadOutcome {
            bytes_written,
            objects_sent: self.pack.map(|p| p.object_count).unwrap_or(0),
            negotiation_rounds: self.negotiation_rounds,
            pack: self.pack,
        }
    }

    fn absorb(&mut self, other: UploadSession) {
        self.negotiation_rounds = self
            .negotiation_rounds
            .saturating_add(other.negotiation_rounds);
        if let Some(p) = other.pack {
            self.pack = Some(match self.pack {
                None => p,
                Some(existing) => PackStats {
                    object_count: existing.object_count.saturating_add(p.object_count),
                    pack_bytes: existing.pack_bytes.saturating_add(p.pack_bytes),
                    delta_count: existing.delta_count.saturating_add(p.delta_count),
                    verbatim_reuse_bytes: existing
                        .verbatim_reuse_bytes
                        .saturating_add(p.verbatim_reuse_bytes),
                    verbatim_reuse_objects: existing
                        .verbatim_reuse_objects
                        .saturating_add(p.verbatim_reuse_objects),
                    pack_ofs_fixup_objects: existing
                        .pack_ofs_fixup_objects
                        .saturating_add(p.pack_ofs_fixup_objects),
                    pack_ref_from_ofs_objects: existing
                        .pack_ref_from_ofs_objects
                        .saturating_add(p.pack_ref_from_ofs_objects),
                },
            });
        }
    }
}

fn map_repo_cancel(err: ServeError) -> ServeError {
    match err {
        ServeError::Repository(crate::error::Error::Cancelled) => ServeError::Cancelled,
        other => other,
    }
}

/// Serve one fetch session: advertise refs, negotiate, and send a pack.
///
/// The version in [`ServeOptions::protocol`] selects the wire format. With
/// [`ServeOptions::advertise_refs`] only the advertisement is written. With
/// [`ServeOptions::stateless_rpc`] the advertisement is skipped and a single
/// request is answered, which is how smart HTTP drives the exchange.
///
/// Shallow fetches, partial-clone filters, and `want-ref` follow repository
/// upload-pack policy and the capabilities advertised for the session. As in
/// Git, the v2 `object-info` command is advertised and answered only when
/// `transfer.advertiseObjectInfo` is true. With `promisor.advertise` set, the
/// v2 capabilities end with `promisor-remote` naming the repository's promisor
/// remotes, and the names a client accepts are collected in
/// [`ServeOptions::accepted_promisor_remotes`].
///
/// # Parameters
///
/// - `repo`: the repository being served.
/// - `input`: requests from the client.
/// - `output`: where responses are written.
/// - `opts`: framing, protocol version, agent string, hidden refs, and (for v2)
///   [`ServeOptions::server_options`] filled from each command request.
///
/// # Errors
///
/// Returns [`ServeError::NotOurRef`] when the client wants an object outside the
/// allowed set (advertised refs in stateful mode; reachable from ref tips in
/// stateless RPC), [`ServeError::Protocol`] for malformed or unsupported requests,
/// [`ServeError::ObjectMissing`] when a repository with promisor remotes lacks an
/// object the response needs (see below), and I/O or repository errors from
/// reading objects and writing the pack.
///
/// # Repositories with promisor remotes
///
/// A repository that keeps some blobs on a promisor remote (`remote.<name>.promisor`
/// or `extensions.partialClone`), such as a server whose large blobs live on a
/// large-object promisor, plans each pack before sending any of it, with the object
/// walk rather than bitmaps. A promised blob it does not store is left out when the
/// client accepted one of the advertised promisor remotes. Otherwise it is fetched
/// first when [`ServeOptions::lazy_fetch`] (or `uploadpack.lazyFetch`) allows, and
/// the request fails with [`ServeError::ObjectMissing`] when it does not. That error
/// reaches a protocol v2 client as an `ERR` line. A v0 client gets it as an `ERR`
/// line when a want itself is missing, and on side band 3 when the pack walk
/// finds the missing object.
/// Only blobs are handled this way: a missing commit, tree or tag fails the
/// request as before.
pub fn upload_pack(
    repo: &Repository,
    input: &mut dyn Read,
    output: &mut dyn Write,
    opts: &mut ServeOptions,
) -> Result<UploadOutcome> {
    upload_pack_cancellable(repo, input, output, opts, &NeverCancel)
}

/// Serve one fetch session, stopping when `cancel` fires.
///
/// Same as [`upload_pack`], but object enumeration and pack generation poll
/// `cancel` and return [`ServeError::Cancelled`] without writing a pack when it
/// fires.
pub fn upload_pack_cancellable(
    repo: &Repository,
    input: &mut dyn Read,
    output: &mut dyn Write,
    opts: &mut ServeOptions,
    cancel: &dyn Cancel,
) -> Result<UploadOutcome> {
    let stop = CancelSignal::new(cancel);
    stop.poll().map_err(|_| ServeError::Cancelled)?;
    let mut counter = CountingWriter {
        inner: output,
        bytes: 0,
    };
    let result = match opts.protocol {
        ProtocolVersion::V2 => serve_v2(repo, input, &mut counter, opts, &stop),
        ProtocolVersion::V0 | ProtocolVersion::V1 => {
            serve_v0(repo, input, &mut counter, opts, &stop)
        }
    };
    match result {
        Ok(session) => {
            counter.flush()?;
            Ok(session.into_outcome(counter.bytes))
        }
        Err(err) => {
            let err = map_repo_cancel(err);
            match err {
                ServeError::Cancelled => {
                    counter.flush()?;
                    Err(ServeError::Cancelled)
                }
                // Part of the pack is out: an `ERR` line now would land inside it.
                err @ ServeError::PackInterrupted(_) => {
                    counter.flush()?;
                    Err(err)
                }
                // Raised before any pack byte: tell the client why instead of hanging up.
                err @ ServeError::ObjectMissing { .. } if opts.protocol == ProtocolVersion::V2 => {
                    write_line(
                        &mut counter,
                        &format!("ERR {}", client_error_message(repo, &err)),
                    )?;
                    write_flush(&mut counter)?;
                    counter.flush()?;
                    if opts.stateless_rpc {
                        Ok(UploadOutcome {
                            bytes_written: counter.bytes,
                            ..UploadOutcome::default()
                        })
                    } else {
                        Err(err)
                    }
                }
                err if opts.stateless_rpc && opts.protocol == ProtocolVersion::V2 => {
                    let msg = serve_error_client_message(&err);
                    write_line(&mut counter, &format!("ERR {msg}"))?;
                    write_flush(&mut counter)?;
                    counter.flush()?;
                    Ok(UploadOutcome {
                        bytes_written: counter.bytes,
                        ..UploadOutcome::default()
                    })
                }
                err => {
                    counter.flush()?;
                    Err(err)
                }
            }
        }
    }
}

/// [`serve_error_client_message`], naming the advertised promisor remotes when
/// the client could have fetched a missing object from one of them.
fn client_error_message(repo: &Repository, err: &ServeError) -> String {
    let msg = serve_error_client_message(err);
    if !matches!(
        err,
        ServeError::ObjectMissing {
            reason: MissingObjectReason::Promised,
            ..
        }
    ) {
        return msg;
    }
    match PromisorAdvertisement::from_server_config(&repo_config(repo)) {
        Some(adv) => format!(
            "{msg} (promisor remotes offered: {})",
            adv.names().collect::<Vec<_>>().join(", ")
        ),
        None => msg,
    }
}

fn serve_error_client_message(err: &ServeError) -> String {
    match err {
        ServeError::ObjectMissing { oid, reason } => match reason {
            MissingObjectReason::Promised => format!(
                "object {oid} lives on a promisor remote this client did not accept; \
                 accept it on the client (promisor.acceptFromServer) or let the server \
                 fetch it (uploadpack.lazyFetch)"
            ),
            MissingObjectReason::NotPromised => {
                format!("object {oid} is missing from the repository")
            }
            MissingObjectReason::NoFetcher => format!(
                "object {oid} is missing and this server cannot fetch from its promisor remotes"
            ),
            MissingObjectReason::LazyFetchFailed => format!(
                "object {oid} is missing and the server's promisor remotes did not supply it"
            ),
        },
        ServeError::Protocol(msg) => msg.clone(),
        ServeError::PackInterrupted(inner) => serve_error_client_message(inner),
        ServeError::UploadFilter(e) => e.to_string(),
        ServeError::NotOurRef(oid) => format!("not our ref {}", oid.to_hex()),
        ServeError::UnknownCommand(cmd) => format!("unknown command {cmd}"),
        ServeError::InvalidDeepenNot { spec, reason } => format!("deepen-not {spec}: {reason}"),
        ServeError::ObjectFormatMismatch { client, repository } => {
            format!("object format mismatch: client wants {client}, repository uses {repository}")
        }
        ServeError::Repository(e) => e.to_string(),
        ServeError::Io(e) => e.to_string(),
        ServeError::Cancelled => "cancelled".to_owned(),
    }
}

fn repo_config(repo: &Repository) -> ConfigSet {
    ConfigSet::load(repo.environment(), Some(repo.git_dir()), true).unwrap_or_default()
}

fn upload_policy(repo: &Repository) -> UploadPackPolicy {
    UploadPackPolicy::from_config(&repo_config(repo))
}

/// How this request treats objects the repository lacks; `None` for a repository
/// without promisor remotes (the ordinary pack path).
///
/// Promised blobs are left out for a client that accepted a promisor remote, and,
/// among the objects the walk reaches, for one whose `filter` leaves them out anyway
/// ([`crate::promisor_remote::request_filter_omits_promised_blobs`]). Over stateless
/// HTTP the second is what lets Git clients fetch: Git sends its `promisor-remote`
/// reply with `ls-refs` only, so the `fetch` request carries no acceptance. Pass
/// `None` for the wants themselves, which are sent whatever the filter says.
fn missing_objects_mode(
    repo: &Repository,
    config: &ConfigSet,
    policy: &UploadPackPolicy,
    opts: &ServeOptions,
    filter: Option<&ObjectFilter>,
) -> Option<MissingObjects> {
    let omitted_by_filter = filter
        .is_some_and(|f| crate::promisor_remote::request_filter_omits_promised_blobs(config, f));
    missing_objects_policy(
        crate::promisor::repo_treats_promisor_packs(repo.git_dir(), config),
        !opts.accepted_promisor_remotes.is_empty() || omitted_by_filter,
        opts.lazy_fetch.unwrap_or(policy.lazy_fetch),
    )
}

/// Tip object ids and annotated tags collected while serving upload-pack.
struct UploadAdvertisement {
    tip_objects: HashSet<ObjectId>,
    tag_candidates: Vec<TagCandidate>,
}

impl UploadAdvertisement {
    fn record(&mut self, r: AdvertisedRefView<'_>) {
        self.tip_objects.insert(r.oid);
        if let Some(peeled) = r.peeled {
            self.tip_objects.insert(peeled);
            if r.name.starts_with("refs/tags/") {
                self.tag_candidates
                    .push(TagCandidate { tag: r.oid, peeled });
            }
        }
    }

    fn collect(repo: &Repository, hidden: &[String]) -> Result<Self> {
        let mut summary = Self {
            tip_objects: HashSet::new(),
            tag_candidates: Vec::new(),
        };
        for_each_advertised_ref(repo, hidden, &[], Peel::Yes, |r| {
            summary.record(r);
            Ok(ControlFlow::Continue(()))
        })?;
        Ok(summary)
    }
}

fn serve_v0(
    repo: &Repository,
    input: &mut dyn Read,
    output: &mut dyn Write,
    opts: &ServeOptions,
    stop: &CancelSignal<'_>,
) -> Result<UploadSession> {
    stop.poll().map_err(|_| ServeError::Cancelled)?;
    let config = repo_config(repo);
    let policy = UploadPackPolicy::from_config(&config);
    let head = head_info(repo);
    let mut advertisement = if opts.advertise_refs || !opts.stateless_rpc {
        if opts.protocol == ProtocolVersion::V1 {
            write_line(output, "version 1")?;
        }
        let summary = write_v0_advertisement(repo, output, &head, opts, &opts.hidden_refs)?;
        if opts.advertise_refs {
            return Ok(UploadSession::default());
        }
        output.flush()?;
        summary
    } else {
        UploadAdvertisement::collect(repo, &opts.hidden_refs)?
    };

    if let Some(h) = head.oid {
        advertisement.tip_objects.insert(h);
    }

    let Some(mut request) = read_v0_request(repo, input, &config, &policy, stop)? else {
        return Ok(UploadSession::default());
    };
    // Wanted objects are sent whatever the filter says, so only acceptance (or
    // lazy fetch) covers a missing want.
    if let Some(mode) = missing_objects_mode(repo, &config, &policy, opts, None) {
        if let Err(err) = resolve_missing_wants(repo, &mut request.wants, mode) {
            if matches!(err, ServeError::ObjectMissing { .. }) {
                // The client reads ACK/NAK next; an `ERR` line there is reported
                // as a remote error, as for Git's own refusals of a want.
                write_line(output, &format!("ERR {}", client_error_message(repo, &err)))?;
                output.flush()?;
            }
            return Err(err);
        }
    }
    let missing = missing_objects_mode(repo, &config, &policy, opts, request.filter.as_ref());
    if request.filter.is_some() {
        validate_wants_filtered(repo, &request.wants)?;
    } else {
        validate_wants(
            repo,
            &advertisement.tip_objects,
            head.oid,
            &request.wants,
            &policy,
            opts.stateless_rpc,
        )?;
    }

    let shallow_resp = if request.shallow.is_active() {
        Some(compute_shallow_response(
            repo,
            &request.wants,
            &request.shallow,
        )?)
    } else {
        None
    };
    if !opts.stateless_rpc {
        if let Some(ref sh) = shallow_resp {
            write_v0_shallow_lines(output, sh)?;
            write_flush(output)?;
            output.flush()?;
        }
    }

    let detailed = request.caps.contains("multi_ack_detailed");
    let wants = &request.wants;
    let mut common: Vec<ObjectId> = Vec::new();
    let mut seen: HashSet<ObjectId> = HashSet::new();
    let mut round_got_common = false;
    let mut round_got_other = false;
    let mut round_last_common: Option<ObjectId> = None;
    let mut negotiation_rounds = 0u32;
    loop {
        stop.poll().map_err(|_| ServeError::Cancelled)?;
        match read_packet(input)? {
            None => {
                if opts.stateless_rpc {
                    if let Some(ref sh) = shallow_resp {
                        write_v0_shallow_lines(output, sh)?;
                        write_flush(output)?;
                    }
                }
                return Ok(UploadSession {
                    negotiation_rounds,
                    ..UploadSession::default()
                });
            }
            Some(Packet::Data(line)) if line == "done" => break,
            Some(Packet::Data(line)) => {
                let Some(hex) = line.strip_prefix("have ") else {
                    return Err(ServeError::Protocol(format!(
                        "expected have or done, got '{line}'"
                    )));
                };
                let oid = parse_oid(repo, hex.trim())?;
                if !seen.insert(oid) {
                    continue;
                }
                if !repo.odb().contains(&oid)? {
                    if detailed {
                        round_got_other = true;
                        if ready_to_send_pack(repo, wants, &common)? {
                            write_line(output, &format!("ACK {} ready", oid.to_hex()))?;
                        }
                    }
                    continue;
                }
                common.push(oid);
                round_got_common = true;
                round_last_common = Some(oid);
                if detailed {
                    write_line(output, &format!("ACK {} common", oid.to_hex()))?;
                } else if common.len() == 1 {
                    write_line(output, &format!("ACK {}", oid.to_hex()))?;
                }
            }
            Some(Packet::Flush) => {
                negotiation_rounds = negotiation_rounds.saturating_add(1);
                if detailed {
                    if round_got_common
                        && !round_got_other
                        && ready_to_send_pack(repo, wants, &common)?
                    {
                        if let Some(last) = round_last_common {
                            write_line(output, &format!("ACK {} ready", last.to_hex()))?;
                        }
                    }
                    write_line(output, "NAK")?;
                    round_got_common = false;
                    round_got_other = false;
                    round_last_common = None;
                } else if common.is_empty() {
                    write_line(output, "NAK")?;
                }
                output.flush()?;
            }
            Some(other) => {
                return Err(ServeError::Protocol(format!(
                    "unexpected packet during negotiation: {other:?}"
                )))
            }
        }
    }

    if opts.stateless_rpc {
        if let Some(ref sh) = shallow_resp {
            write_v0_shallow_lines(output, sh)?;
            write_flush(output)?;
        }
    }
    match common.last() {
        Some(last) => write_line(output, &format!("ACK {}", last.to_hex()))?,
        None => write_line(output, "NAK")?,
    }
    output.flush()?;
    let sideband = request.caps.contains("side-band-64k");
    stop.poll().map_err(|_| ServeError::Cancelled)?;
    let pack = write_response_pack_from_v0(
        repo,
        &advertisement.tag_candidates,
        &request,
        &common,
        shallow_resp.as_ref(),
        missing,
        output,
        sideband,
        stop,
    )?;
    Ok(UploadSession {
        pack: Some(pack),
        negotiation_rounds,
    })
}

/// Write the v0 ref advertisement and record tip ids for want validation.
fn write_v0_advertisement(
    repo: &Repository,
    output: &mut dyn Write,
    head: &super::HeadInfo,
    opts: &ServeOptions,
    hidden: &[String],
) -> Result<UploadAdvertisement> {
    let policy = upload_policy(repo);
    let mut caps: Vec<String> = V0_CAPABILITIES.iter().map(|c| (*c).to_owned()).collect();
    for cap in policy.v0_capability_tokens() {
        if !caps.iter().any(|existing| existing == cap) {
            caps.push(cap.to_owned());
        }
    }
    if let (Some(target), Some(_)) = (&head.target, head.oid) {
        caps.push(format!("symref=HEAD:{target}"));
    }
    caps.push(format!("object-format={}", repo.odb().hash_algo().name()));
    if !opts.agent.is_empty() {
        caps.push(format!("agent={}", opts.agent));
    }
    let caps = caps.join(" ");

    let mut summary = UploadAdvertisement {
        tip_objects: HashSet::new(),
        tag_candidates: Vec::new(),
    };
    let mut first_line = true;
    if let Some(oid) = head.oid {
        summary.tip_objects.insert(oid);
        write_line(output, &format!("{} HEAD\0{caps}", oid.to_hex()))?;
        first_line = false;
    }
    let mut wrote_ref = !first_line;
    for_each_advertised_ref(repo, hidden, &[], Peel::Yes, |r| {
        summary.record(r);
        if first_line {
            write_line(output, &format!("{} {}\0{caps}", r.oid.to_hex(), r.name))?;
            first_line = false;
        } else {
            write_line(output, &format!("{} {}", r.oid.to_hex(), r.name))?;
        }
        wrote_ref = true;
        if let Some(peeled) = r.peeled {
            write_line(output, &format!("{} {}^{{}}", peeled.to_hex(), r.name))?;
        }
        Ok(ControlFlow::Continue(()))
    })?;
    if !wrote_ref {
        let zero = ObjectId::null(repo.odb().hash_algo());
        write_line(
            output,
            &format!("{} capabilities^{{}}\0{caps}", zero.to_hex()),
        )?;
    }
    write_flush(output)?;
    Ok(summary)
}

struct V0Request {
    wants: Vec<ObjectId>,
    caps: HashSet<String>,
    shallow: ShallowRequest,
    filter: Option<ObjectFilter>,
}

fn read_v0_request(
    repo: &Repository,
    input: &mut dyn Read,
    config: &ConfigSet,
    policy: &UploadPackPolicy,
    stop: &CancelSignal<'_>,
) -> Result<Option<V0Request>> {
    let mut wants = Vec::new();
    let mut caps = HashSet::new();
    let mut shallow = ShallowRequest::default();
    let mut filter: Option<ObjectFilter> = None;
    loop {
        stop.poll().map_err(|_| ServeError::Cancelled)?;
        match read_packet(input)? {
            None | Some(Packet::Flush) => break,
            Some(Packet::Data(line)) => {
                if let Some(rest) = line.strip_prefix("want ") {
                    let mut parts = rest.split(' ');
                    let hex = parts.next().unwrap_or_default();
                    wants.push(parse_oid(repo, hex)?);
                    if wants.len() == 1 {
                        caps.extend(parts.filter(|c| !c.is_empty()).map(str::to_owned));
                    }
                    continue;
                }
                if parse_shallow_line(repo, &line, &mut shallow, config, policy, &mut filter)? {
                    continue;
                }
                return Err(ServeError::Protocol(format!(
                    "unsupported request line '{line}'"
                )));
            }
            Some(other) => {
                return Err(ServeError::Protocol(format!(
                    "unexpected packet in want list: {other:?}"
                )))
            }
        }
    }
    if wants.is_empty() {
        return Ok(None);
    }
    if caps.contains("deepen-relative") {
        shallow.deepen_relative = true;
    }
    let format = caps.iter().find_map(|c| c.strip_prefix("object-format="));
    check_object_format(repo, format)?;
    Ok(Some(V0Request {
        wants,
        caps,
        shallow,
        filter,
    }))
}

fn parse_shallow_line(
    repo: &Repository,
    line: &str,
    shallow: &mut ShallowRequest,
    config: &ConfigSet,
    policy: &UploadPackPolicy,
    filter: &mut Option<ObjectFilter>,
) -> Result<bool> {
    if let Some(rest) = line.strip_prefix("shallow ") {
        shallow.client_shallow.push(parse_oid(repo, rest.trim())?);
        return Ok(true);
    }
    if let Some(rest) = line.strip_prefix("deepen ") {
        shallow.depth = Some(
            rest.trim()
                .parse()
                .map_err(|_| ServeError::Protocol(format!("invalid deepen value '{rest}'")))?,
        );
        return Ok(true);
    }
    if let Some(rest) = line.strip_prefix("deepen-since ") {
        shallow.deepen_since = Some(
            rest.trim()
                .parse()
                .map_err(|_| ServeError::Protocol(format!("invalid deepen-since '{rest}'")))?,
        );
        return Ok(true);
    }
    if let Some(rest) = line.strip_prefix("deepen-not ") {
        shallow
            .deepen_not
            .push(resolve_deepen_not(repo, rest.trim())?);
        return Ok(true);
    }
    if line == "deepen-relative" {
        shallow.deepen_relative = true;
        return Ok(true);
    }
    if let Some(rest) = line.strip_prefix("filter ") {
        let spec = rest.trim();
        let parsed = policy.parse_filter(config, spec)?;
        *filter = Some(match filter.take() {
            Some(existing) => existing.merge_with(parsed),
            None => parsed,
        });
        return Ok(true);
    }
    Ok(false)
}

/// Resolve a `deepen-not` argument to the commit that bounds the shallow history.
///
/// Git clients send whatever the user gave `--shallow-exclude`, usually a ref
/// name such as `v1.0` or `refs/heads/main`. A full-width object id is accepted
/// too. Short names expand through the standard ref lookup rules and must match
/// exactly one ref. Annotated tags are peeled.
fn resolve_deepen_not(repo: &Repository, spec: &str) -> Result<ObjectId> {
    let invalid = |reason| ServeError::InvalidDeepenNot {
        spec: spec.to_owned(),
        reason,
    };
    let oid = match parse_oid(repo, spec) {
        Ok(oid) => oid,
        Err(_) => {
            let (matches, name) = crate::refs::shorten::expand_ref_dwim_full_name(repo, spec)?;
            let name = match (matches, name) {
                (1, Some(name)) => name,
                (0, _) | (_, None) => return Err(invalid(DeepenNotError::UnknownRef)),
                _ => return Err(invalid(DeepenNotError::AmbiguousRef)),
            };
            repo.resolve_ref_oid(&name)
                .map_err(|_| invalid(DeepenNotError::UnknownRef))?
        }
    };
    if !repo.odb().contains(&oid)? {
        return Err(invalid(DeepenNotError::UnknownRef));
    }
    match repo.odb().peel_tags(oid)? {
        (commit, ObjectKind::Commit) => Ok(commit),
        _ => Err(invalid(DeepenNotError::NotACommit)),
    }
}

fn write_v0_shallow_lines(output: &mut dyn Write, sh: &ShallowResponse) -> Result<()> {
    for oid in &sh.shallow {
        write_line(output, &format!("shallow {}", oid.to_hex()))?;
    }
    for oid in &sh.unshallow {
        write_line(output, &format!("unshallow {}", oid.to_hex()))?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn write_response_pack_from_v0(
    repo: &Repository,
    include_tags: &[TagCandidate],
    request: &V0Request,
    common: &[ObjectId],
    shallow: Option<&ShallowResponse>,
    missing: Option<MissingObjects>,
    output: &mut dyn Write,
    sideband: bool,
    stop: &CancelSignal<'_>,
) -> Result<PackStats> {
    let mut pack_wants = request.wants.clone();
    if let Some(sh) = shallow {
        pack_wants.extend(sh.extra_wants.iter().copied());
    }
    pack_wants.sort();
    pack_wants.dedup();

    let pack_common: Vec<ObjectId> = if let Some(sh) = shallow {
        common
            .iter()
            .filter(|oid| !sh.unshallow.contains(oid))
            .copied()
            .collect()
    } else {
        common.to_vec()
    };

    let have_shallow: HashSet<ObjectId> = request.shallow.client_shallow.iter().copied().collect();
    let source_shallow = shallow
        .map(|s| s.pack_shallow_grafts.clone())
        .unwrap_or_default();

    let input = ResponsePackInput {
        repo,
        include_tags,
        wants: &pack_wants,
        common: &pack_common,
        caps: &request.caps,
        filter: request.filter.as_ref(),
        have_shallow: &have_shallow,
        source_shallow: &source_shallow,
        missing,
    };
    let prepared = match prepare_response_pack(&input) {
        Ok(prepared) => prepared,
        Err(err @ ServeError::ObjectMissing { .. }) if sideband => {
            // Nothing of the pack is out yet: say why on the error band.
            let message = format!("{}\n", client_error_message(repo, &err));
            let _ = crate::pkt_line::write_sideband_packet(output, 3, message.as_bytes())
                .and_then(|()| output.flush());
            return Err(err);
        }
        Err(err) => return Err(err),
    };
    write_prepared_pack(&input, &prepared, output, sideband, stop)
}

fn serve_v2(
    repo: &Repository,
    input: &mut dyn Read,
    output: &mut dyn Write,
    opts: &mut ServeOptions,
    stop: &CancelSignal<'_>,
) -> Result<UploadSession> {
    stop.poll().map_err(|_| ServeError::Cancelled)?;
    let config = repo_config(repo);
    let policy = UploadPackPolicy::from_config(&config);
    let promisor_adv = PromisorAdvertisement::from_server_config(&config);
    if opts.advertise_refs || !opts.stateless_rpc {
        write_line(output, "version 2")?;
        if !opts.agent.is_empty() {
            write_line(output, &format!("agent={}", opts.agent))?;
        }
        write_line(output, "ls-refs=unborn")?;
        if opts.bundle_list.is_some() {
            write_line(output, "bundle-uri")?;
        }
        let fetch_caps = policy.v2_fetch_features().join(" ");
        write_line(output, &format!("fetch={fetch_caps}"))?;
        write_line(output, "server-option")?;
        write_line(
            output,
            &format!("object-format={}", repo.odb().hash_algo().name()),
        )?;
        if policy.advertise_object_info {
            // Name the attributes `object-info` answers, as Git 2.56+ does;
            // clients match the capability by key, so older ones still see it.
            write_line(output, "object-info=size type")?;
        }
        if let Some(adv) = &promisor_adv {
            write_line(
                output,
                &format!(
                    "{}={}",
                    crate::protocol_v2::PROMISOR_REMOTE_CAPABILITY,
                    adv.to_capability_value()
                ),
            )?;
        }
        write_flush(output)?;
        if opts.advertise_refs {
            return Ok(UploadSession::default());
        }
        output.flush()?;
    }

    let mut aggregate = UploadSession::default();
    loop {
        stop.poll().map_err(|_| ServeError::Cancelled)?;
        opts.server_options.clear();
        let Some(request) = read_v2_request(input, opts, promisor_adv.as_ref())? else {
            return Ok(aggregate);
        };
        check_object_format(repo, request.object_format.as_deref())?;
        let session = match request.command.as_str() {
            "ls-refs" => {
                ls_refs(repo, output, &request.args, opts)?;
                UploadSession::default()
            }
            "fetch" => fetch_v2(repo, output, &request.args, opts, stop)?,
            "object-info" if policy.advertise_object_info => {
                super::object_info::write_object_info_response(repo, output, &request.args)?;
                UploadSession::default()
            }
            "bundle-uri" if opts.bundle_list.is_some() => {
                bundle_uri(output, opts)?;
                UploadSession::default()
            }
            other => return Err(ServeError::UnknownCommand(other.to_owned())),
        };
        output.flush()?;
        if opts.stateless_rpc {
            return Ok(session);
        }
        aggregate.absorb(session);
    }
}

/// One v2 command request.
struct V2Request {
    command: String,
    object_format: Option<String>,
    args: Vec<String>,
}

/// Read a v2 command request. Returns `None` at end of input or on a bare
/// flush, which ends the session.
///
/// A `promisor-remote=<names>` capability records the names this server
/// advertised in [`ServeOptions::accepted_promisor_remotes`]; other names are
/// ignored, and a reply without names is a protocol error.
fn read_v2_request(
    input: &mut dyn Read,
    opts: &mut ServeOptions,
    promisor_adv: Option<&PromisorAdvertisement>,
) -> Result<Option<V2Request>> {
    let mut command = None;
    let mut object_format = None;
    let mut args = Vec::new();
    let mut in_args = false;
    loop {
        match read_packet(input)? {
            None => {
                if command.is_none() {
                    return Ok(None);
                }
                return Err(ServeError::Protocol("truncated command request".into()));
            }
            Some(Packet::Flush) => break,
            Some(Packet::Delim) if !in_args => in_args = true,
            Some(Packet::Data(line)) if in_args => {
                if let Some(value) = line.strip_prefix("server-option ") {
                    opts.server_options.push(value.to_owned());
                } else {
                    args.push(line);
                }
            }
            Some(Packet::Data(line)) => {
                if let Some(c) = line.strip_prefix("command=") {
                    command = Some(c.to_owned());
                } else if let Some(f) = line.strip_prefix("object-format=") {
                    object_format = Some(f.to_owned());
                } else if let Some(opt) = line.strip_prefix("server-option=") {
                    opts.server_options.push(opt.to_owned());
                } else if let Some(rest) =
                    line.strip_prefix(crate::protocol_v2::PROMISOR_REMOTE_CAPABILITY)
                {
                    record_accepted_promisors(rest, opts, promisor_adv)?;
                }
                // agent= and unknown capabilities are ignored.
            }
            Some(other) => {
                return Err(ServeError::Protocol(format!(
                    "unexpected packet in command request: {other:?}"
                )))
            }
        }
    }
    match command {
        Some(command) => Ok(Some(V2Request {
            command,
            object_format,
            args,
        })),
        None if args.is_empty() => Ok(None),
        None => Err(ServeError::Protocol(
            "command request without command".into(),
        )),
    }
}

/// Record the client's `promisor-remote` reply (`rest` is what follows the
/// capability name) in `opts`, keeping only names this server advertised.
fn record_accepted_promisors(
    rest: &str,
    opts: &mut ServeOptions,
    promisor_adv: Option<&PromisorAdvertisement>,
) -> Result<()> {
    let Some(value) = rest.strip_prefix('=') else {
        if rest.is_empty() {
            return Err(ServeError::Protocol(
                "promisor-remote capability without a value".into(),
            ));
        }
        // Some other capability that merely starts with the same letters.
        return Ok(());
    };
    let names = parse_accepted_reply(value).map_err(|e| ServeError::Protocol(e.to_string()))?;
    let Some(adv) = promisor_adv else {
        return Ok(());
    };
    for name in names {
        if adv.names().any(|n| n == name) && !opts.accepted_promisor_remotes.contains(&name) {
            opts.accepted_promisor_remotes.push(name);
        }
    }
    Ok(())
}

/// Answer `command=bundle-uri` with the configured bundle list.
fn bundle_uri(output: &mut dyn Write, opts: &ServeOptions) -> Result<()> {
    if let Some(list) = &opts.bundle_list {
        for line in list.to_lines() {
            write_line(output, &line)?;
        }
    }
    write_flush(output)
}

fn ls_refs(
    repo: &Repository,
    output: &mut dyn Write,
    args: &[String],
    opts: &ServeOptions,
) -> Result<()> {
    let mut symrefs = false;
    let mut peel = Peel::No;
    let mut unborn = false;
    let mut prefixes: Vec<&str> = Vec::new();
    for arg in args {
        match arg.as_str() {
            "symrefs" => symrefs = true,
            "peel" => peel = Peel::Yes,
            "unborn" => unborn = true,
            other => {
                if let Some(p) = other.strip_prefix("ref-prefix ") {
                    prefixes.push(p);
                }
            }
        }
    }
    if prefixes.is_empty() || prefixes.iter().any(|p| "HEAD".starts_with(p)) {
        let head = head_info(repo);
        let target = head.target.as_deref().filter(|_| symrefs);
        match (head.oid, target) {
            (Some(oid), Some(t)) => {
                write_line(output, &format!("{} HEAD symref-target:{t}", oid.to_hex()))?
            }
            (Some(oid), None) => write_line(output, &format!("{} HEAD", oid.to_hex()))?,
            (None, Some(t)) if unborn => {
                write_line(output, &format!("unborn HEAD symref-target:{t}"))?
            }
            (None, _) => {}
        }
    }
    for_each_advertised_ref(repo, &opts.hidden_refs, &prefixes, peel, |r| {
        let mut line = format!("{} {}", r.oid.to_hex(), r.name);
        if let Some(p) = r.peeled {
            line.push_str(&format!(" peeled:{}", p.to_hex()));
        }
        write_line(output, &line)?;
        Ok(ControlFlow::Continue(()))
    })?;
    write_flush(output)
}

fn fetch_v2(
    repo: &Repository,
    output: &mut dyn Write,
    args: &[String],
    opts: &mut ServeOptions,
    stop: &CancelSignal<'_>,
) -> Result<UploadSession> {
    stop.poll().map_err(|_| ServeError::Cancelled)?;
    let config = repo_config(repo);
    let policy = UploadPackPolicy::from_config(&config);
    let mut wants = Vec::new();
    let mut haves = Vec::new();
    let mut done = false;
    let mut caps = HashSet::new();
    let mut deepen = None;
    let mut deepen_since = None;
    let mut deepen_not: Vec<ObjectId> = Vec::new();
    let mut deepen_relative = false;
    let mut filter_spec: Option<String> = None;
    let mut want_ref_names: Vec<String> = Vec::new();
    let mut wait_for_done = false;
    for arg in args {
        if let Some(hex) = arg.strip_prefix("want ") {
            wants.push(parse_oid(repo, hex.trim())?);
        } else if let Some(hex) = arg.strip_prefix("have ") {
            haves.push(parse_oid(repo, hex.trim())?);
        } else if arg.starts_with("shallow ") {
        } else if let Some(rest) = arg.strip_prefix("deepen ") {
            deepen = Some(
                rest.trim()
                    .parse()
                    .map_err(|_| ServeError::Protocol(format!("bad deepen '{arg}'")))?,
            );
        } else if let Some(rest) = arg.strip_prefix("deepen-since ") {
            deepen_since = Some(
                rest.trim()
                    .parse()
                    .map_err(|_| ServeError::Protocol(format!("bad deepen-since '{arg}'")))?,
            );
        } else if let Some(rest) = arg.strip_prefix("deepen-not ") {
            deepen_not.push(resolve_deepen_not(repo, rest.trim())?);
        } else if let Some(spec) = arg.strip_prefix("filter ") {
            filter_spec = Some(spec.trim().to_owned());
        } else if let Some(name) = arg.strip_prefix("want-ref ").filter(|_| policy.ref_in_want) {
            let name = name.trim();
            if want_ref_names.iter().any(|n| n == name) {
                return Err(ServeError::Protocol(format!("duplicate want-ref {name}")));
            }
            want_ref_names.push(name.to_owned());
        } else if arg == "done" {
            done = true;
        } else if arg == "wait-for-done" {
            wait_for_done = true;
        } else if arg == "deepen-relative" {
            deepen_relative = true;
        } else if let Some(value) = arg.strip_prefix("server-option ") {
            opts.server_options.push(value.to_owned());
        } else if matches!(
            arg.as_str(),
            "thin-pack" | "no-progress" | "include-tag" | "ofs-delta"
        ) || (arg == "sideband-all" && policy.allow_sideband_all)
        {
            caps.insert(arg.clone());
        } else {
            return Err(ServeError::Protocol(format!(
                "unsupported fetch argument '{arg}'"
            )));
        }
    }
    let mut advertisement = UploadAdvertisement::collect(repo, &opts.hidden_refs)?;
    let head = head_info(repo);
    if let Some(h) = head.oid {
        advertisement.tip_objects.insert(h);
    }
    let wanted_refs = want_ref_names
        .into_iter()
        .map(|name| {
            let oid = resolve_want_ref(repo, &opts.hidden_refs, &name)?;
            Ok((name, oid))
        })
        .collect::<Result<Vec<_>>>()?;
    wants.extend(wanted_refs.iter().map(|(_, oid)| *oid));

    let filter: Option<ObjectFilter> = match filter_spec {
        None => None,
        Some(spec) => Some(policy.parse_filter(&config, &spec)?),
    };

    // Wanted objects are sent whatever the filter says, so only acceptance (or
    // lazy fetch) covers a missing want.
    if let Some(mode) = missing_objects_mode(repo, &config, &policy, opts, None) {
        resolve_missing_wants(repo, &mut wants, mode)?;
    }
    let missing = missing_objects_mode(repo, &config, &policy, opts, filter.as_ref());
    if filter.is_some() {
        validate_wants_filtered(repo, &wants)?;
    } else {
        validate_wants(
            repo,
            &advertisement.tip_objects,
            head.oid,
            &wants,
            &policy,
            opts.stateless_rpc,
        )?;
    }

    let sideband_all = caps.contains("sideband-all");
    let mut seen = HashSet::new();
    let common = repo
        .odb()
        .present_oids(haves.into_iter().filter(|h| seen.insert(*h)))?;

    let mut negotiation_rounds = 0u32;
    if !done {
        negotiation_rounds = 1;
        write_v2_fetch_line(output, "acknowledgments", sideband_all)?;
        if common.is_empty() {
            write_v2_fetch_line(output, "NAK", sideband_all)?;
        }
        for oid in &common {
            write_v2_fetch_line(output, &format!("ACK {}", oid.to_hex()), sideband_all)?;
        }
        if wait_for_done || !ready_to_send_pack(repo, &wants, &common)? {
            write_flush(output)?;
            return Ok(UploadSession {
                negotiation_rounds,
                ..UploadSession::default()
            });
        }
        // After `ready` the pack follows in this same response.
        write_v2_fetch_line(output, "ready", sideband_all)?;
        write_v2_section_delim(output)?;
    }

    let client_shallow: Vec<ObjectId> = args
        .iter()
        .filter_map(|a| a.strip_prefix("shallow "))
        .map(|hex| parse_oid(repo, hex.trim()))
        .collect::<Result<Vec<_>>>()?;

    let shallow_req = ShallowRequest {
        client_shallow: client_shallow.clone(),
        depth: deepen,
        deepen_since,
        deepen_not,
        deepen_relative,
    };
    let shallow_active = shallow_req.is_active();
    let shallow_resp = if shallow_active {
        compute_shallow_response(repo, &wants, &shallow_req)?
    } else {
        Default::default()
    };

    let mut pack_wants = wants;
    pack_wants.extend(shallow_resp.extra_wants.iter().copied());
    pack_wants.sort();
    pack_wants.dedup();
    let have_shallow: HashSet<ObjectId> = client_shallow.iter().copied().collect();
    let input = ResponsePackInput {
        repo,
        include_tags: &advertisement.tag_candidates,
        wants: &pack_wants,
        common: &common,
        caps: &caps,
        filter: filter.as_ref(),
        have_shallow: &have_shallow,
        source_shallow: &shallow_resp.pack_shallow_grafts,
        missing,
    };
    // Settle what the pack holds before any section that commits to sending it.
    let prepared = prepare_response_pack(&input)?;

    if !shallow_resp.shallow.is_empty() || !shallow_resp.unshallow.is_empty() {
        write_v2_fetch_line(output, "shallow-info", sideband_all)?;
        for oid in &shallow_resp.unshallow {
            write_v2_fetch_line(output, &format!("unshallow {}", oid.to_hex()), sideband_all)?;
        }
        for oid in &shallow_resp.shallow {
            write_v2_fetch_line(output, &format!("shallow {}", oid.to_hex()), sideband_all)?;
        }
        write_v2_section_delim(output)?;
    }

    if !wanted_refs.is_empty() {
        write_v2_fetch_line(output, "wanted-refs", sideband_all)?;
        for (name, oid) in &wanted_refs {
            write_v2_fetch_line(output, &format!("{} {name}", oid.to_hex()), sideband_all)?;
        }
        write_v2_section_delim(output)?;
    }

    write_v2_fetch_line(output, "packfile", sideband_all)?;
    stop.poll().map_err(|_| ServeError::Cancelled)?;
    // Protocol v2 always side-band-64k frames the packfile section payload (band 1),
    // even when `sideband-all` was not negotiated.
    let pack = write_prepared_pack(&input, &prepared, output, true, stop)?;
    Ok(UploadSession {
        pack: Some(pack),
        negotiation_rounds,
    })
}

/// Whether a v2 negotiation can stop at this round: some have was acknowledged
/// and every want that peels to a commit reaches the acknowledged commits.
///
/// Wants that do not peel to a commit (trees, blobs) cannot be judged by
/// ancestry and do not hold the negotiation open.
fn ready_to_send_pack(repo: &Repository, wants: &[ObjectId], common: &[ObjectId]) -> Result<bool> {
    if common.is_empty() {
        return Ok(false);
    }
    let odb = repo.odb();
    let mut want_commits = Vec::with_capacity(wants.len());
    for want in wants {
        let (peeled, kind) = odb.peel_tags(*want)?;
        if kind == ObjectKind::Commit {
            want_commits.push(peeled);
        }
    }
    if want_commits.is_empty() {
        return Ok(true);
    }
    let mut have_commits = Vec::new();
    for oid in common {
        if odb.read_info(oid)?.kind == ObjectKind::Commit {
            have_commits.push(*oid);
        }
    }
    Ok(crate::merge_base::wants_covered_by_haves(
        repo,
        &want_commits,
        &have_commits,
    )?)
}

fn write_v2_section_delim(output: &mut dyn Write) -> Result<()> {
    write!(output, "0001").map_err(crate::error::Error::Io)?;
    Ok(())
}

/// Write one v2 fetch response line, using sideband channel 1 when `sideband-all` was negotiated.
fn write_v2_fetch_line(output: &mut dyn Write, line: &str, sideband_all: bool) -> Result<()> {
    if sideband_all {
        let mut payload = Vec::with_capacity(line.len() + 1);
        payload.extend_from_slice(line.as_bytes());
        payload.push(b'\n');
        crate::pkt_line::write_sideband_packet(output, 1, &payload).map_err(ServeError::Io)?;
        Ok(())
    } else {
        write_line(output, line)
    }
}

/// Inputs for [`prepare_response_pack`] and [`write_prepared_pack`].
struct ResponsePackInput<'a> {
    repo: &'a Repository,
    include_tags: &'a [TagCandidate],
    wants: &'a [ObjectId],
    common: &'a [ObjectId],
    caps: &'a HashSet<String>,
    filter: Option<&'a ObjectFilter>,
    have_shallow: &'a HashSet<ObjectId>,
    source_shallow: &'a HashSet<ObjectId>,
    /// How objects the repository lacks are handled; `None` without promisor remotes.
    missing: Option<MissingObjects>,
}

/// A response pack whose contents are settled but not yet written.
struct PreparedPack {
    /// Annotated tags `include-tag` may add; the pack writer adds those whose peeled
    /// object it sends.
    include_tags: Vec<TagCandidate>,
    pack_opts: PackBuildOptions,
    /// The planned objects, for a repository with promisor remotes; otherwise the
    /// pack is enumerated (or reused from bitmaps) while it streams.
    planned: Option<PackObjectSelection>,
}

/// Settle what a response pack holds: the `include-tag` candidates and, for a
/// repository with promisor remotes, the objects with missing blobs omitted or
/// fetched.
///
/// # Errors
///
/// [`ServeError::ObjectMissing`] when a missing blob can be neither omitted nor
/// fetched, and read failures. Nothing has been written to the client then.
fn prepare_response_pack(input: &ResponsePackInput<'_>) -> Result<PreparedPack> {
    let include_tags = if input.caps.contains("include-tag") {
        input.include_tags.to_vec()
    } else {
        Vec::new()
    };
    let cfg = input.repo.config().ok();
    let pack_opts = PackBuildOptions::for_serve(
        cfg.as_deref(),
        input.caps.contains("thin-pack") && !input.common.is_empty(),
        input.caps.contains("ofs-delta"),
    );
    let planned = match input.missing {
        None => None,
        Some(mode) => Some(plan_pack(
            input.repo,
            input.wants,
            input.common,
            input.source_shallow,
            input.have_shallow,
            input.filter,
            &include_tags,
            &pack_opts,
            mode,
        )?),
    };
    Ok(PreparedPack {
        include_tags,
        pack_opts,
        planned,
    })
}

/// Stream the pack [`prepare_response_pack`] settled to `output`.
///
/// With `sideband` the pack is framed on side-band channel 1 and followed by a
/// flush packet. The pack is written as it is produced, never held in memory whole.
///
/// # Errors
///
/// A failure before any pack byte went out is returned as is. One after is
/// [`ServeError::PackInterrupted`]; with a side band, the client is first told
/// on band 3 (best effort: the failure may be the client hanging up).
fn write_prepared_pack(
    input: &ResponsePackInput<'_>,
    prepared: &PreparedPack,
    output: &mut dyn Write,
    sideband: bool,
    stop: &CancelSignal<'_>,
) -> Result<PackStats> {
    stop.poll().map_err(|_| ServeError::Cancelled)?;
    let mut progress = |_ev: PackProgress| {};
    let mut send = |out: &mut dyn Write| match &prepared.planned {
        Some(selection) => {
            write_upload_pack_selection(input.repo, selection, &prepared.pack_opts, out, stop)
        }
        None => write_upload_pack_stream_with_cancel(
            input.repo,
            input.wants,
            input.common,
            input.source_shallow,
            input.have_shallow,
            input.filter,
            &prepared.include_tags,
            &prepared.pack_opts,
            &mut progress,
            out,
            stop,
        ),
    };
    if !sideband {
        let mut counted = CountingWriter {
            inner: output,
            bytes: 0,
        };
        return match send(&mut counted) {
            Ok(stats) => Ok(stats),
            Err(e) => Err(pack_failure(e, counted.bytes)),
        };
    }
    let sent = {
        let mut band = SidebandChannel1Writer::new(&mut *output);
        let mut counted = CountingWriter {
            inner: &mut band,
            bytes: 0,
        };
        let result = send(&mut counted);
        let written = counted.bytes;
        match result {
            Ok(stats) => band
                .finish()
                .map(|()| stats)
                .map_err(|e| pack_failure_io(e, written)),
            Err(e) => Err(pack_failure(e, written)),
        }
    };
    match sent {
        Ok(stats) => {
            write_flush(output)?;
            Ok(stats)
        }
        Err(ServeError::PackInterrupted(err)) => {
            // hygiene: band 3 carries the remote error line Git clients print verbatim
            let message = format!("error: {}\n", serve_error_client_message(&err));
            let _ = crate::pkt_line::write_sideband_packet(output, 3, message.as_bytes())
                .and_then(|()| output.flush());
            Err(ServeError::PackInterrupted(err))
        }
        Err(err) => Err(err),
    }
}

/// Classify a pack-writing failure: before any byte went out it is returned
/// as is (a cancel stays a cancel), after it is [`ServeError::PackInterrupted`].
fn pack_failure(err: crate::error::Error, written: u64) -> ServeError {
    let err = map_repo_cancel(ServeError::Repository(err));
    if written == 0 || matches!(err, ServeError::Cancelled) {
        return err;
    }
    ServeError::PackInterrupted(Box::new(err))
}

/// [`pack_failure`] for an I/O error from finishing the side band.
fn pack_failure_io(err: std::io::Error, written: u64) -> ServeError {
    if written == 0 {
        return ServeError::Io(err);
    }
    ServeError::PackInterrupted(Box::new(ServeError::Io(err)))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::config::environment::RepositoryOptions;
    use crate::repo::InitOptions;
    use crate::serve::AdvertisedRef;

    /// Bare repository with `n` linear commits (oldest first) and an annotated
    /// tag `refs/tags/t<i>` on every commit index in `tagged`.
    struct Fixture {
        _dir: tempfile::TempDir,
        repo: Repository,
        commits: Vec<ObjectId>,
        tags: Vec<AdvertisedRef>,
    }

    fn fixture(n: usize, tagged: &[usize]) -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        Repository::init(dir.path(), &InitOptions::new().with_bare(true)).unwrap();
        let repo = Repository::open(dir.path(), &RepositoryOptions::empty()).unwrap();
        let (commits, tags) = write_history(&repo, n, tagged);
        Fixture {
            _dir: dir,
            repo,
            commits,
            tags,
        }
    }

    /// Write `n` linear commits and the annotated tags `t<i>` for `tagged` into `repo`.
    fn write_history(
        repo: &Repository,
        n: usize,
        tagged: &[usize],
    ) -> (Vec<ObjectId>, Vec<AdvertisedRef>) {
        let odb = repo.odb();
        let tree = odb.write(ObjectKind::Tree, b"").unwrap();
        let mut commits: Vec<ObjectId> = Vec::new();
        for i in 0..n {
            let parent = commits
                .last()
                .map(|p| format!("parent {p}\n"))
                .unwrap_or_default();
            let body = format!(
                "tree {tree}\n{parent}author T <t@x> {i} +0000\ncommitter T <t@x> {i} +0000\n\nc{i}\n"
            );
            commits.push(odb.write(ObjectKind::Commit, body.as_bytes()).unwrap());
        }
        let tags = tagged
            .iter()
            .map(|&i| {
                let body = format!(
                    "object {}\ntype commit\ntag t{i}\ntagger T <t@x> 0 +0000\n\nt\n",
                    commits[i]
                );
                AdvertisedRef {
                    name: format!("refs/tags/t{i}"),
                    oid: odb.write(ObjectKind::Tag, body.as_bytes()).unwrap(),
                    peeled: Some(commits[i]),
                }
            })
            .collect();
        (commits, tags)
    }

    /// Prepare and write the pack answering `input`, as a fetch does.
    fn write_response_pack(
        input: ResponsePackInput<'_>,
        output: &mut dyn Write,
        sideband: bool,
        stop: &CancelSignal<'_>,
    ) -> Result<PackStats> {
        let prepared = prepare_response_pack(&input)?;
        write_prepared_pack(&input, &prepared, output, sideband, stop)
    }

    fn tag_candidates_from(ads: &[AdvertisedRef]) -> Vec<TagCandidate> {
        ads.iter()
            .filter_map(|r| r.peeled.map(|peeled| TagCandidate { tag: r.oid, peeled }))
            .collect()
    }

    /// Answer `input` with `include-tag` and return the tags of `input.include_tags` the
    /// pack holds, sorted.
    fn packed_tags(input: ResponsePackInput<'_>) -> Vec<ObjectId> {
        let tag_ids: Vec<ObjectId> = input.include_tags.iter().map(|t| t.tag).collect();
        let mut pack = Vec::new();
        write_response_pack(input, &mut pack, false, &CancelSignal::never()).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let unpacked = crate::odb::Odb::new(dir.path());
        crate::unpack_objects::unpack_objects(
            &mut pack.as_slice(),
            &unpacked,
            &crate::unpack_objects::UnpackOptions::default(),
        )
        .unwrap();
        let mut tags: Vec<ObjectId> = tag_ids
            .into_iter()
            .filter(|oid| unpacked.contains(oid).unwrap())
            .collect();
        tags.sort();
        tags
    }

    fn depth_request(depth: u32) -> ShallowRequest {
        ShallowRequest {
            depth: Some(depth),
            ..ShallowRequest::default()
        }
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn shallow_response_for_tag_wants_names_only_commits() {
        let fx = fixture(5, &[1, 4]);
        let tip = fx.commits[4];
        let wants = [tip, fx.tags[0].oid, fx.tags[1].oid];
        let resp = compute_shallow_response(&fx.repo, &wants, &depth_request(1)).unwrap();
        let mut shallow = resp.shallow.clone();
        shallow.sort();
        let mut expected = vec![fx.commits[1], tip];
        expected.sort();
        assert_eq!(shallow, expected);
        for oid in &resp.pack_shallow_grafts {
            assert_eq!(
                fx.repo.odb().read_info(oid).unwrap().kind,
                ObjectKind::Commit
            );
        }
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn shallow_response_ignores_wants_that_are_not_commits() {
        let fx = fixture(2, &[]);
        let tree = fx
            .repo
            .odb()
            .write_loose_materialize(ObjectKind::Tree, b"")
            .unwrap();
        let resp =
            compute_shallow_response(&fx.repo, &[fx.commits[1], tree], &depth_request(1)).unwrap();
        assert_eq!(resp.shallow, vec![fx.commits[1]]);
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn included_tags_stop_at_the_shallow_boundary() {
        let fx = fixture(6, &[1, 3, 5]);
        let tip = fx.commits[5];
        let resp = compute_shallow_response(&fx.repo, &[tip], &depth_request(3)).unwrap();
        let caps = HashSet::from(["include-tag".to_owned()]);
        let include_tags = tag_candidates_from(&fx.tags);
        let tags = packed_tags(ResponsePackInput {
            repo: &fx.repo,
            include_tags: &include_tags,
            wants: &[tip],
            common: &[],
            caps: &caps,
            filter: None,
            have_shallow: &HashSet::new(),
            source_shallow: &resp.pack_shallow_grafts,
            missing: None,
        });
        let mut expected = vec![fx.tags[1].oid, fx.tags[2].oid];
        expected.sort();
        assert_eq!(tags, expected, "t1 lies beyond the depth-3 cut");
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn included_tags_skip_history_the_client_already_has() {
        let fx = fixture(6, &[1, 3, 5]);
        let caps = HashSet::from(["include-tag".to_owned()]);
        let include_tags = tag_candidates_from(&fx.tags);
        let tags = packed_tags(ResponsePackInput {
            repo: &fx.repo,
            include_tags: &include_tags,
            wants: &[fx.commits[5]],
            common: &[fx.commits[3]],
            caps: &caps,
            filter: None,
            have_shallow: &HashSet::new(),
            source_shallow: &HashSet::new(),
            missing: None,
        });
        assert_eq!(tags, vec![fx.tags[2].oid]);
    }

    #[test]
    fn include_tag_reads_follow_the_fetch_not_the_history() {
        let store = crate::odb::store::flaky::FlakyStore::new();
        let dir = tempfile::tempdir().unwrap();
        let git_dir = dir.path().join("repo.git");
        let repo = crate::repo::RepositoryBuilder::new(RepositoryOptions::empty())
            .odb_builder(
                crate::odb::OdbBuilder::files(git_dir.join("objects"))
                    .primary(store.clone())
                    .alternates(false),
            )
            .ref_store(std::sync::Arc::new(
                crate::refs::store::MemoryRefStore::new(),
            ))
            .open(&git_dir, None)
            .unwrap();
        let n = 3000;
        let (commits, tags) = write_history(&repo, n, &[5, n - 1]);
        let caps = HashSet::from(["include-tag".to_owned()]);
        let include_tags = tag_candidates_from(&tags);
        let before = store.reads();
        let packed = packed_tags(ResponsePackInput {
            repo: &repo,
            include_tags: &include_tags,
            wants: &[commits[n - 1]],
            common: &[commits[n - 3]],
            caps: &caps,
            filter: None,
            have_shallow: &HashSet::new(),
            source_shallow: &HashSet::new(),
            missing: None,
        });
        let reads = store.reads() - before;
        assert_eq!(packed, vec![tags[1].oid], "only the tag on a sent commit");
        assert!(
            reads < 50,
            "a two-commit fetch read {reads} objects from a {n}-commit history"
        );
    }

    #[test]
    fn include_tag_follows_tag_chains_and_non_commit_targets() {
        let fx = fixture(3, &[2]);
        let odb = fx.repo.odb();
        let tag = |target: ObjectId, kind: &str, name: &str| {
            let body =
                format!("object {target}\ntype {kind}\ntag {name}\ntagger T <t@x> 0 +0000\n\nt\n");
            odb.write(ObjectKind::Tag, body.as_bytes()).unwrap()
        };
        let inner = fx.tags[0].oid;
        let outer = tag(inner, "tag", "outer");
        let blob = odb.write(ObjectKind::Blob, b"new\n").unwrap();
        let mut tree_body = b"100644 f\0".to_vec();
        tree_body.extend_from_slice(blob.as_bytes());
        let tree = odb.write(ObjectKind::Tree, &tree_body).unwrap();
        let tip = odb
            .write(
                ObjectKind::Commit,
                format!(
                    "tree {tree}\nparent {}\nauthor T <t@x> 3 +0000\ncommitter T <t@x> 3 +0000\n\nc3\n",
                    fx.commits[2]
                )
                .as_bytes(),
            )
            .unwrap();
        let blob_tag = tag(blob, "blob", "blob");
        let old_tag = tag(fx.commits[0], "commit", "old");
        let tag_ads = vec![
            AdvertisedRef {
                name: "refs/tags/outer".to_owned(),
                oid: outer,
                peeled: Some(fx.commits[2]),
            },
            AdvertisedRef {
                name: "refs/tags/blob".to_owned(),
                oid: blob_tag,
                peeled: Some(blob),
            },
            AdvertisedRef {
                name: "refs/tags/old".to_owned(),
                oid: old_tag,
                peeled: Some(fx.commits[0]),
            },
        ];
        let include_tags = tag_candidates_from(&tag_ads);
        let caps = HashSet::from(["include-tag".to_owned()]);
        let fetch = |wants: &[ObjectId]| {
            let mut pack = Vec::new();
            write_response_pack(
                ResponsePackInput {
                    repo: &fx.repo,
                    include_tags: &include_tags,
                    wants,
                    common: &[fx.commits[1]],
                    caps: &caps,
                    filter: None,
                    have_shallow: &HashSet::new(),
                    source_shallow: &HashSet::new(),
                    missing: None,
                },
                &mut pack,
                false,
                &CancelSignal::never(),
            )
            .unwrap();
            let dir = tempfile::tempdir().unwrap();
            let unpacked = crate::odb::Odb::new(dir.path());
            let count = crate::unpack_objects::unpack_objects(
                &mut pack.as_slice(),
                &unpacked,
                &crate::unpack_objects::UnpackOptions::default(),
            )
            .unwrap();
            (dir, unpacked, count)
        };

        let (_dir, unpacked, count) = fetch(&[tip]);
        for (oid, what) in [
            (outer, "the followed tag"),
            (inner, "the tag it names"),
            (blob_tag, "a tag of a sent blob"),
        ] {
            assert!(unpacked.contains(&oid).unwrap(), "{what} is packed");
        }
        assert!(
            !unpacked.contains(&old_tag).unwrap(),
            "a tag on history the client has is not packed"
        );
        assert_eq!(
            count, 7,
            "two commits, tree, blob, both chain tags, blob tag"
        );

        let (_dir, unpacked, count) = fetch(&[inner]);
        assert!(unpacked.contains(&outer).unwrap());
        assert!(
            !unpacked.contains(&blob_tag).unwrap(),
            "its blob is not sent"
        );
        assert_eq!(count, 3, "the wanted inner tag is packed once");
    }

    /// Write a commit with an empty tree, `parents`, and committer time `time`.
    fn commit_at(repo: &Repository, parents: &[ObjectId], time: i64) -> ObjectId {
        let tree = repo
            .odb()
            .write_loose_materialize(ObjectKind::Tree, b"")
            .unwrap();
        let parents: String = parents.iter().map(|p| format!("parent {p}\n")).collect();
        let body = format!(
            "tree {tree}\n{parents}author T <t@x> {time} +0000\ncommitter T <t@x> {time} +0000\n\nc{time}\n"
        );
        repo.odb()
            .write_loose_materialize(ObjectKind::Commit, body.as_bytes())
            .unwrap()
    }

    #[test]
    fn ready_needs_an_acknowledged_have() {
        let fx = fixture(3, &[]);
        assert!(!ready_to_send_pack(&fx.repo, &[fx.commits[2]], &[]).unwrap());
        assert!(ready_to_send_pack(&fx.repo, &[fx.commits[2]], &[fx.commits[0]]).unwrap());
    }

    #[test]
    fn ready_when_the_want_is_a_have_or_its_parent() {
        let fx = fixture(3, &[]);
        let [c0, c1, c2] = [fx.commits[0], fx.commits[1], fx.commits[2]];
        assert!(ready_to_send_pack(&fx.repo, &[c2], &[c2]).unwrap());
        assert!(ready_to_send_pack(&fx.repo, &[c1], &[c2]).unwrap());
        assert!(!ready_to_send_pack(&fx.repo, &[c0], &[c2]).unwrap());
    }

    #[test]
    fn ready_walk_stops_at_commits_older_than_the_oldest_have() {
        let fx = fixture(1, &[]);
        let base = fx.commits[0];
        // `skewed` is older than the have, so the walk from `tip` stops there
        // even though `skewed`'s parent is the have's parent.
        let skewed = commit_at(&fx.repo, &[base], 10);
        let tip = commit_at(&fx.repo, &[skewed], 300);
        let recent_have = commit_at(&fx.repo, &[base], 200);
        assert!(!ready_to_send_pack(&fx.repo, &[tip], &[recent_have]).unwrap());
        let old_have = commit_at(&fx.repo, &[base], 5);
        assert!(ready_to_send_pack(&fx.repo, &[tip], &[old_have]).unwrap());
    }

    #[test]
    fn ready_requires_every_commit_want_but_not_tree_wants() {
        let fx = fixture(6, &[3]);
        let side = commit_at(&fx.repo, &[fx.commits[2]], 50);
        let have = [fx.commits[0]];
        assert!(ready_to_send_pack(&fx.repo, &[fx.commits[5], fx.commits[3]], &have).unwrap());
        assert!(ready_to_send_pack(&fx.repo, &[fx.tags[0].oid], &have).unwrap());
        assert!(ready_to_send_pack(&fx.repo, &[side, fx.commits[4]], &have).unwrap());

        let have = [fx.commits[5]];
        assert!(!ready_to_send_pack(&fx.repo, &[fx.commits[4], side], &have).unwrap());
        assert!(!ready_to_send_pack(&fx.repo, &[side, fx.commits[4]], &have).unwrap());
        let tree = fx
            .repo
            .odb()
            .write_loose_materialize(ObjectKind::Tree, b"")
            .unwrap();
        assert!(ready_to_send_pack(&fx.repo, &[tree, fx.commits[4]], &have).unwrap());
    }
}
