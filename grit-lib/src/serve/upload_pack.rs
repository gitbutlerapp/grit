//! Serving fetches and clones (`upload-pack`).

use super::shallow_upload::{compute_shallow_response, ShallowRequest, ShallowResponse};
use super::upload_pack_policy::{
    resolve_want_ref, validate_wants, validate_wants_filtered, UploadPackPolicy,
};
use super::{
    advertised_refs, check_object_format, head_info, parse_oid, read_packet, write_flush,
    write_line, AdvertisedRef, DeepenNotError, Packet, Peel, ProtocolVersion, Result, ServeError,
    ServeOptions,
};
use crate::config::ConfigSet;
use crate::objects::pack_objects::PackStats;
use crate::objects::{ObjectId, ObjectKind};
use crate::pack_object_select::TagCandidate;
use crate::pack_objects::{write_upload_pack_stream_with_cancel, PackProgress};
use crate::pkt_line::SidebandChannel1Writer;
use crate::progress::{Cancel, CancelSignal, NeverCancel};
use crate::repo::Repository;
use crate::rev_list::ObjectFilter;
use crate::transfer::PackBuildOptions;
use std::collections::HashSet;
use std::io::{Read, Write};

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
/// `transfer.advertiseObjectInfo` is true.
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
/// and I/O or repository errors from reading objects and writing the pack.
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

fn serve_error_client_message(err: &ServeError) -> String {
    match err {
        ServeError::Protocol(msg) => msg.clone(),
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
    let refs = advertised_refs(repo, &opts.hidden_refs, &[], Peel::Yes)?;
    let head = head_info(repo);

    if opts.advertise_refs || !opts.stateless_rpc {
        if opts.protocol == ProtocolVersion::V1 {
            write_line(output, "version 1")?;
        }
        write_v0_advertisement(repo, output, &refs, &head, opts)?;
        if opts.advertise_refs {
            return Ok(UploadSession::default());
        }
        output.flush()?;
    }

    let Some(request) = read_v0_request(repo, input, &config, &policy, stop)? else {
        return Ok(UploadSession::default());
    };
    if request.filter.is_some() {
        validate_wants_filtered(repo, &request.wants)?;
    } else {
        validate_wants(
            repo,
            &refs,
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
        &refs,
        &request,
        &common,
        request.filter.as_ref(),
        shallow_resp.as_ref(),
        output,
        sideband,
        stop,
    )?;
    Ok(UploadSession {
        pack: Some(pack),
        negotiation_rounds,
    })
}

/// Write the v0 ref advertisement: `HEAD` and every ref, capabilities on the
/// first line, and a peeled `^{}` line after each annotated tag.
fn write_v0_advertisement(
    repo: &Repository,
    output: &mut dyn Write,
    refs: &[AdvertisedRef],
    head: &super::HeadInfo,
    opts: &ServeOptions,
) -> Result<()> {
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

    let mut lines: Vec<(ObjectId, String)> = Vec::new();
    if let Some(oid) = head.oid {
        lines.push((oid, "HEAD".to_owned()));
    }
    for r in refs {
        lines.push((r.oid, r.name.clone()));
        if let Some(peeled) = r.peeled {
            lines.push((peeled, format!("{}^{{}}", r.name)));
        }
    }

    if lines.is_empty() {
        let zero = ObjectId::null(repo.odb().hash_algo());
        write_line(
            output,
            &format!("{} capabilities^{{}}\0{caps}", zero.to_hex()),
        )?;
    }
    for (i, (oid, name)) in lines.iter().enumerate() {
        if i == 0 {
            write_line(output, &format!("{} {name}\0{caps}", oid.to_hex()))?;
        } else {
            write_line(output, &format!("{} {name}", oid.to_hex()))?;
        }
    }
    write_flush(output)
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
    refs: &[AdvertisedRef],
    request: &V0Request,
    common: &[ObjectId],
    filter: Option<&ObjectFilter>,
    shallow: Option<&ShallowResponse>,
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

    write_response_pack(
        ResponsePackInput {
            repo,
            refs,
            wants: &pack_wants,
            common: &pack_common,
            caps: &request.caps,
            filter,
            have_shallow: &have_shallow,
            source_shallow: &source_shallow,
        },
        output,
        sideband,
        stop,
    )
}

fn serve_v2(
    repo: &Repository,
    input: &mut dyn Read,
    output: &mut dyn Write,
    opts: &mut ServeOptions,
    stop: &CancelSignal<'_>,
) -> Result<UploadSession> {
    stop.poll().map_err(|_| ServeError::Cancelled)?;
    let policy = upload_policy(repo);
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
        let Some(request) = read_v2_request(input, opts)? else {
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
fn read_v2_request(input: &mut dyn Read, opts: &mut ServeOptions) -> Result<Option<V2Request>> {
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
    for r in advertised_refs(repo, &opts.hidden_refs, &prefixes, peel)? {
        let mut line = format!("{} {}", r.oid.to_hex(), r.name);
        if let Some(p) = r.peeled {
            line.push_str(&format!(" peeled:{}", p.to_hex()));
        }
        write_line(output, &line)?;
    }
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
    let refs = advertised_refs(repo, &opts.hidden_refs, &[], Peel::Yes)?;
    let head = head_info(repo);
    let wanted_refs = want_ref_names
        .into_iter()
        .map(|name| {
            let oid = resolve_want_ref(repo, &refs, &opts.hidden_refs, &name)?;
            Ok((name, oid))
        })
        .collect::<Result<Vec<_>>>()?;
    wants.extend(wanted_refs.iter().map(|(_, oid)| *oid));

    let filter: Option<ObjectFilter> = match filter_spec {
        None => None,
        Some(spec) => Some(policy.parse_filter(&config, &spec)?),
    };

    if filter.is_some() {
        validate_wants_filtered(repo, &wants)?;
    } else {
        validate_wants(repo, &refs, head.oid, &wants, &policy, opts.stateless_rpc)?;
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

    let mut pack_wants = wants;
    pack_wants.extend(shallow_resp.extra_wants);
    pack_wants.sort();
    pack_wants.dedup();

    let have_shallow: HashSet<ObjectId> = client_shallow.iter().copied().collect();
    write_v2_fetch_line(output, "packfile", sideband_all)?;
    stop.poll().map_err(|_| ServeError::Cancelled)?;
    // Protocol v2 always side-band-64k frames the packfile section payload (band 1),
    // even when `sideband-all` was not negotiated.
    let pack = write_response_pack(
        ResponsePackInput {
            repo,
            refs: &refs,
            wants: &pack_wants,
            common: &common,
            caps: &caps,
            filter: filter.as_ref(),
            have_shallow: &have_shallow,
            source_shallow: &shallow_resp.pack_shallow_grafts,
        },
        output,
        true,
        stop,
    )?;
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

/// Inputs for [`write_response_pack`].
struct ResponsePackInput<'a> {
    repo: &'a Repository,
    refs: &'a [AdvertisedRef],
    wants: &'a [ObjectId],
    common: &'a [ObjectId],
    caps: &'a HashSet<String>,
    filter: Option<&'a ObjectFilter>,
    have_shallow: &'a HashSet<ObjectId>,
    source_shallow: &'a HashSet<ObjectId>,
}

/// Stream the pack answering a fetch to `output`, adding annotated tags when
/// `include-tag` was negotiated.
///
/// With `sideband` the pack is framed on side-band channel 1 and followed by a
/// flush packet. The pack is written as it is produced, never held in memory whole.
fn write_response_pack(
    input: ResponsePackInput<'_>,
    output: &mut dyn Write,
    sideband: bool,
    stop: &CancelSignal<'_>,
) -> Result<PackStats> {
    stop.poll().map_err(|_| ServeError::Cancelled)?;
    let include_tags = if input.caps.contains("include-tag") {
        tag_candidates(input.refs)
    } else {
        Vec::new()
    };
    let cfg = input.repo.config().ok();
    let pack_opts = PackBuildOptions::for_serve(
        cfg.as_deref(),
        input.caps.contains("thin-pack") && !input.common.is_empty(),
        input.caps.contains("ofs-delta"),
    );
    let mut progress = |_ev: PackProgress| {};
    let stats = if !sideband {
        write_upload_pack_stream_with_cancel(
            input.repo,
            input.wants,
            input.common,
            input.source_shallow,
            input.have_shallow,
            input.filter,
            &include_tags,
            &pack_opts,
            &mut progress,
            output,
            stop,
        )
        .map_err(ServeError::Repository)?
    } else {
        let mut band = SidebandChannel1Writer::new(output);
        let stats = write_upload_pack_stream_with_cancel(
            input.repo,
            input.wants,
            input.common,
            input.source_shallow,
            input.have_shallow,
            input.filter,
            &include_tags,
            &pack_opts,
            &mut progress,
            &mut band,
            stop,
        )
        .map_err(ServeError::Repository)?;
        band.finish()?;
        write_flush(output)?;
        stats
    };
    Ok(stats)
}

/// The annotated tags under `refs/tags/` that `include-tag` may add to the pack.
///
/// The pack writer adds a tag only when the object it peels to (a commit, tree or
/// blob) is among the objects being sent, so tags on history the client already has
/// or beyond a shallow boundary stay out, and no history is walked to decide it.
fn tag_candidates(refs: &[AdvertisedRef]) -> Vec<TagCandidate> {
    refs.iter()
        .filter(|r| r.name.starts_with("refs/tags/"))
        .filter_map(|r| r.peeled.map(|peeled| TagCandidate { tag: r.oid, peeled }))
        .collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::config::environment::RepositoryOptions;
    use crate::repo::InitOptions;

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

    /// Answer `input` with `include-tag` and return the tags of `input.refs` the
    /// pack holds, sorted.
    fn packed_tags(input: ResponsePackInput<'_>) -> Vec<ObjectId> {
        let refs = input.refs;
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
        let mut tags: Vec<ObjectId> = refs
            .iter()
            .map(|r| r.oid)
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
        let tags = packed_tags(ResponsePackInput {
            repo: &fx.repo,
            refs: &fx.tags,
            wants: &[tip],
            common: &[],
            caps: &caps,
            filter: None,
            have_shallow: &HashSet::new(),
            source_shallow: &resp.pack_shallow_grafts,
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
        let tags = packed_tags(ResponsePackInput {
            repo: &fx.repo,
            refs: &fx.tags,
            wants: &[fx.commits[5]],
            common: &[fx.commits[3]],
            caps: &caps,
            filter: None,
            have_shallow: &HashSet::new(),
            source_shallow: &HashSet::new(),
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
        let before = store.reads();
        let packed = packed_tags(ResponsePackInput {
            repo: &repo,
            refs: &tags,
            wants: &[commits[n - 1]],
            common: &[commits[n - 3]],
            caps: &caps,
            filter: None,
            have_shallow: &HashSet::new(),
            source_shallow: &HashSet::new(),
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
        let refs = vec![
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
        let caps = HashSet::from(["include-tag".to_owned()]);
        let fetch = |wants: &[ObjectId]| {
            let mut pack = Vec::new();
            write_response_pack(
                ResponsePackInput {
                    repo: &fx.repo,
                    refs: &refs,
                    wants,
                    common: &[fx.commits[1]],
                    caps: &caps,
                    filter: None,
                    have_shallow: &HashSet::new(),
                    source_shallow: &HashSet::new(),
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
