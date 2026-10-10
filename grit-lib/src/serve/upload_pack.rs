//! Serving fetches and clones (`upload-pack`).

use std::collections::{HashSet, VecDeque};
use std::io::{Read, Write};

use super::shallow_upload::{compute_shallow_response, ShallowRequest, ShallowResponse};
use super::upload_pack_policy::{
    resolve_want_ref, validate_wants, validate_wants_filtered, UploadPackPolicy,
};
use super::{
    advertised_refs, check_object_format, head_info, parse_oid, read_packet, write_flush,
    write_line, AdvertisedRef, Packet, ProtocolVersion, Result, ServeError, ServeOptions,
};
use crate::config::ConfigSet;
use crate::objects::{ObjectId, ObjectKind};
use crate::pack_objects::build_pack_for_upload;
use crate::repo::Repository;
use crate::rev_list::ObjectFilter;
use crate::transfer::PackBuildOptions;

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

/// Serve one fetch session: advertise refs, negotiate, and send a pack.
///
/// The version in [`ServeOptions::protocol`] selects the wire format. With
/// [`ServeOptions::advertise_refs`] only the advertisement is written. With
/// [`ServeOptions::stateless_rpc`] the advertisement is skipped and a single
/// request is answered, which is how smart HTTP drives the exchange.
///
/// Shallow fetches, partial-clone filters, and `want-ref` follow repository
/// upload-pack policy and the capabilities advertised for the session.
///
/// # Parameters
///
/// - `repo`: the repository being served.
/// - `input`: requests from the client.
/// - `output`: where responses are written.
/// - `opts`: framing, protocol version, agent string, and hidden refs.
///
/// # Errors
///
/// Returns [`ServeError::NotOurRef`] when the client wants an object that was not
/// advertised, [`ServeError::Protocol`] for malformed or unsupported requests,
/// and I/O or repository errors from reading objects and writing the pack.
pub fn upload_pack(
    repo: &Repository,
    input: &mut dyn Read,
    output: &mut dyn Write,
    opts: &ServeOptions,
) -> Result<()> {
    let result = match opts.protocol {
        ProtocolVersion::V2 => serve_v2(repo, input, output, opts),
        ProtocolVersion::V0 | ProtocolVersion::V1 => serve_v0(repo, input, output, opts),
    };
    match result {
        Ok(()) => {
            output.flush()?;
            Ok(())
        }
        Err(err) if opts.stateless_rpc && opts.protocol == ProtocolVersion::V2 => {
            let msg = serve_error_client_message(&err);
            write_line(output, &format!("ERR {msg}"))?;
            write_flush(output)?;
            output.flush()?;
            Ok(())
        }
        Err(err) => {
            output.flush()?;
            Err(err)
        }
    }
}

fn serve_error_client_message(err: &ServeError) -> String {
    match err {
        ServeError::Protocol(msg) => msg.clone(),
        ServeError::UploadFilter(e) => e.to_string(),
        ServeError::NotOurRef(oid) => format!("not our ref {}", oid.to_hex()),
        ServeError::UnknownCommand(cmd) => format!("unknown command {cmd}"),
        ServeError::ObjectFormatMismatch { client, repository } => {
            format!("object format mismatch: client wants {client}, repository uses {repository}")
        }
        ServeError::Repository(e) => e.to_string(),
        ServeError::Io(e) => e.to_string(),
    }
}

fn repo_config(repo: &Repository) -> ConfigSet {
    ConfigSet::load(repo.environment(), Some(&repo.git_dir), true).unwrap_or_default()
}

fn upload_policy(repo: &Repository) -> UploadPackPolicy {
    UploadPackPolicy::from_config(&repo_config(repo))
}

fn serve_v0(
    repo: &Repository,
    input: &mut dyn Read,
    output: &mut dyn Write,
    opts: &ServeOptions,
) -> Result<()> {
    let config = repo_config(repo);
    let policy = UploadPackPolicy::from_config(&config);
    let refs = advertised_refs(repo, &opts.hidden_refs)?;
    let head = head_info(repo);

    if opts.advertise_refs || !opts.stateless_rpc {
        if opts.protocol == ProtocolVersion::V1 {
            write_line(output, "version 1")?;
        }
        write_v0_advertisement(repo, output, &refs, &head, opts)?;
        if opts.advertise_refs {
            return Ok(());
        }
        output.flush()?;
    }

    let Some(request) = read_v0_request(repo, input, &config, &policy)? else {
        return Ok(());
    };
    if request.filter.is_some() {
        validate_wants_filtered(repo, &request.wants)?;
    } else {
        validate_wants(repo, &refs, head.oid, &request.wants, &policy)?;
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
    let mut common: Vec<ObjectId> = Vec::new();
    let mut seen: HashSet<ObjectId> = HashSet::new();
    loop {
        match read_packet(input)? {
            None => {
                if opts.stateless_rpc {
                    if let Some(ref sh) = shallow_resp {
                        write_v0_shallow_lines(output, sh)?;
                        write_flush(output)?;
                    }
                }
                return Ok(());
            }
            Some(Packet::Data(line)) if line == "done" => break,
            Some(Packet::Data(line)) => {
                let Some(hex) = line.strip_prefix("have ") else {
                    return Err(ServeError::Protocol(format!(
                        "expected have or done, got '{line}'"
                    )));
                };
                let oid = parse_oid(repo, hex.trim())?;
                if !seen.insert(oid) || !repo.odb.exists(&oid) {
                    continue;
                }
                common.push(oid);
                if detailed {
                    write_line(output, &format!("ACK {} common", oid.to_hex()))?;
                } else if common.len() == 1 {
                    write_line(output, &format!("ACK {}", oid.to_hex()))?;
                }
            }
            Some(Packet::Flush) => {
                if detailed || common.is_empty() {
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
        Some(last) if detailed => write_line(output, &format!("ACK {}", last.to_hex()))?,
        Some(_) => {}
        None => write_line(output, "NAK")?,
    }
    let sideband = request.caps.contains("side-band-64k");
    let pack = build_response_pack_from_v0(
        repo,
        &refs,
        &request,
        &common,
        request.filter.as_ref(),
        shallow_resp.as_ref(),
    )?;
    write_pack(output, &pack, sideband)
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
    caps.push(format!("object-format={}", repo.odb.hash_algo().name()));
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
        let zero = ObjectId::null(repo.odb.hash_algo());
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
) -> Result<Option<V0Request>> {
    let mut wants = Vec::new();
    let mut caps = HashSet::new();
    let mut shallow = ShallowRequest::default();
    let mut filter: Option<ObjectFilter> = None;
    loop {
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

fn resolve_deepen_not(repo: &Repository, spec: &str) -> Result<ObjectId> {
    if let Ok(oid) = ObjectId::from_hex(spec) {
        return Ok(oid);
    }
    crate::refs::resolve_ref(&repo.git_dir, spec)
        .map_err(|_| ServeError::Protocol(format!("invalid deepen-not '{spec}'")))
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

fn build_response_pack_from_v0(
    repo: &Repository,
    refs: &[AdvertisedRef],
    request: &V0Request,
    common: &[ObjectId],
    filter: Option<&ObjectFilter>,
    shallow: Option<&ShallowResponse>,
) -> Result<Vec<u8>> {
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

    build_response_pack(ResponsePackInput {
        repo,
        refs,
        wants: &pack_wants,
        common: &pack_common,
        caps: &request.caps,
        filter,
        have_shallow: &have_shallow,
        source_shallow: &source_shallow,
    })
}

fn serve_v2(
    repo: &Repository,
    input: &mut dyn Read,
    output: &mut dyn Write,
    opts: &ServeOptions,
) -> Result<()> {
    if opts.advertise_refs || !opts.stateless_rpc {
        write_line(output, "version 2")?;
        if !opts.agent.is_empty() {
            write_line(output, &format!("agent={}", opts.agent))?;
        }
        write_line(output, "ls-refs=unborn")?;
        let policy = upload_policy(repo);
        let fetch_caps = policy.v2_fetch_features().join(" ");
        write_line(output, &format!("fetch={fetch_caps}"))?;
        write_line(
            output,
            &format!("object-format={}", repo.odb.hash_algo().name()),
        )?;
        write_flush(output)?;
        if opts.advertise_refs {
            return Ok(());
        }
        output.flush()?;
    }

    loop {
        let Some(request) = read_v2_request(input)? else {
            return Ok(());
        };
        check_object_format(repo, request.object_format.as_deref())?;
        match request.command.as_str() {
            "ls-refs" => ls_refs(repo, output, &request.args, opts)?,
            "fetch" => fetch_v2(repo, output, &request.args, opts)?,
            other => return Err(ServeError::UnknownCommand(other.to_owned())),
        }
        output.flush()?;
        if opts.stateless_rpc {
            return Ok(());
        }
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
fn read_v2_request(input: &mut dyn Read) -> Result<Option<V2Request>> {
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
            Some(Packet::Data(line)) if in_args => args.push(line),
            Some(Packet::Data(line)) => {
                if let Some(c) = line.strip_prefix("command=") {
                    command = Some(c.to_owned());
                } else if let Some(f) = line.strip_prefix("object-format=") {
                    object_format = Some(f.to_owned());
                }
                // agent=, server-option= and unknown capabilities are ignored.
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

fn ls_refs(
    repo: &Repository,
    output: &mut dyn Write,
    args: &[String],
    opts: &ServeOptions,
) -> Result<()> {
    let mut symrefs = false;
    let mut peel = false;
    let mut unborn = false;
    let mut prefixes: Vec<&str> = Vec::new();
    for arg in args {
        match arg.as_str() {
            "symrefs" => symrefs = true,
            "peel" => peel = true,
            "unborn" => unborn = true,
            other => {
                if let Some(p) = other.strip_prefix("ref-prefix ") {
                    prefixes.push(p);
                }
            }
        }
    }
    let wanted = |name: &str| prefixes.is_empty() || prefixes.iter().any(|p| name.starts_with(p));

    if wanted("HEAD") {
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
    for r in advertised_refs(repo, &opts.hidden_refs)? {
        if !wanted(&r.name) {
            continue;
        }
        let mut line = format!("{} {}", r.oid.to_hex(), r.name);
        if let (true, Some(p)) = (peel, r.peeled) {
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
    opts: &ServeOptions,
) -> Result<()> {
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
            deepen_not.push(parse_oid(repo, rest.trim())?);
        } else if let Some(spec) = arg.strip_prefix("filter ") {
            filter_spec = Some(spec.trim().to_owned());
        } else if let Some(name) = arg.strip_prefix("want-ref ") {
            want_ref_names.push(name.trim().to_owned());
        } else if arg == "done" {
            done = true;
        } else if arg == "deepen-relative" {
            deepen_relative = true;
        } else if matches!(
            arg.as_str(),
            "thin-pack" | "no-progress" | "include-tag" | "ofs-delta" | "sideband-all"
        ) {
            caps.insert(arg.clone());
        } else {
            return Err(ServeError::Protocol(format!(
                "unsupported fetch argument '{arg}'"
            )));
        }
    }

    let refs = advertised_refs(repo, &opts.hidden_refs)?;
    let head = head_info(repo);
    for name in want_ref_names {
        wants.push(resolve_want_ref(repo, &refs, &opts.hidden_refs, &name)?);
    }

    let filter: Option<ObjectFilter> = match filter_spec {
        None => None,
        Some(spec) => Some(policy.parse_filter(&config, &spec)?),
    };

    if filter.is_some() {
        validate_wants_filtered(repo, &wants)?;
    } else {
        validate_wants(repo, &refs, head.oid, &wants, &policy)?;
    }

    let sideband_all = caps.contains("sideband-all");
    let mut seen = HashSet::new();
    let common: Vec<ObjectId> = haves
        .into_iter()
        .filter(|h| seen.insert(*h) && repo.odb.exists(h))
        .collect();

    if !done {
        write_v2_fetch_line(output, "acknowledgments", sideband_all)?;
        if common.is_empty() {
            write_v2_fetch_line(output, "NAK", sideband_all)?;
        }
        for oid in &common {
            write_v2_fetch_line(output, &format!("ACK {}", oid.to_hex()), sideband_all)?;
        }
        return write_flush(output);
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

    let mut pack_wants = wants;
    pack_wants.extend(shallow_resp.extra_wants);
    pack_wants.sort();
    pack_wants.dedup();

    let have_shallow: HashSet<ObjectId> = client_shallow.iter().copied().collect();
    let pack = build_response_pack(ResponsePackInput {
        repo,
        refs: &refs,
        wants: &pack_wants,
        common: &common,
        caps: &caps,
        filter: filter.as_ref(),
        have_shallow: &have_shallow,
        source_shallow: &shallow_resp.pack_shallow_grafts,
    })?;
    write_v2_fetch_line(output, "packfile", sideband_all)?;
    // Protocol v2 always side-band-64k frames the packfile section payload (band 1),
    // even when `sideband-all` was not negotiated.
    write_pack(output, &pack, true)
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

/// Inputs for [`build_response_pack`].
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

/// Build the pack answering a fetch, adding annotated tags when `include-tag`
/// was negotiated.
fn build_response_pack(input: ResponsePackInput<'_>) -> Result<Vec<u8>> {
    let mut pack_wants = input.wants.to_vec();
    if input.caps.contains("include-tag") {
        pack_wants.extend(included_tags(
            input.repo,
            input.refs,
            input.wants,
            input.common,
        )?);
    }
    pack_wants.sort();
    pack_wants.dedup();

    let pack_opts = PackBuildOptions {
        thin: input.caps.contains("thin-pack"),
        delta: true,
        use_ofs_delta: input.caps.contains("ofs-delta"),
        ..PackBuildOptions::default()
    };
    Ok(build_pack_for_upload(
        input.repo,
        &pack_wants,
        input.common,
        input.have_shallow,
        input.source_shallow,
        input.filter,
        &pack_opts,
    )?)
}

/// Annotated tags the client did not ask for but whose target commit is being
/// sent, so the client can follow tags without a second request.
///
/// Only tags that peel to commits are considered.
fn included_tags(
    repo: &Repository,
    refs: &[AdvertisedRef],
    wants: &[ObjectId],
    common: &[ObjectId],
) -> Result<Vec<ObjectId>> {
    let want_set: HashSet<ObjectId> = wants.iter().copied().collect();
    let candidates: Vec<&AdvertisedRef> = refs
        .iter()
        .filter(|r| r.name.starts_with("refs/tags/") && r.peeled.is_some())
        .filter(|r| !want_set.contains(&r.oid))
        .collect();
    if candidates.is_empty() {
        return Ok(Vec::new());
    }
    let have_commits = commit_closure(repo, common, &HashSet::new())?;
    let sent_commits = commit_closure(repo, wants, &have_commits)?;
    Ok(candidates
        .into_iter()
        .filter(|r| r.peeled.is_some_and(|p| sent_commits.contains(&p)))
        .map(|r| r.oid)
        .collect())
}

/// Commits reachable from `tips` (peeling tags), not descending into `stop`.
fn commit_closure(
    repo: &Repository,
    tips: &[ObjectId],
    stop: &HashSet<ObjectId>,
) -> Result<HashSet<ObjectId>> {
    let mut seen = HashSet::new();
    let mut queue: VecDeque<ObjectId> = VecDeque::new();
    for tip in tips {
        let Ok(oid) = super::peel_tag(repo, *tip) else {
            continue;
        };
        queue.push_back(oid);
    }
    while let Some(oid) = queue.pop_front() {
        if stop.contains(&oid) || !seen.insert(oid) {
            continue;
        }
        let Ok(obj) = repo.odb.read(&oid) else {
            continue;
        };
        if obj.kind != ObjectKind::Commit {
            seen.remove(&oid);
            continue;
        }
        let commit = crate::objects::parse_commit(&obj.data)?;
        queue.extend(commit.parents);
    }
    Ok(seen)
}

/// Write a pack, framed on side-band channel 1 when negotiated, then a flush.
fn write_pack(output: &mut dyn Write, pack: &[u8], sideband: bool) -> Result<()> {
    if sideband {
        crate::pkt_line::write_sideband_channel1_64k(&mut &mut *output, pack)?;
        write_flush(output)
    } else {
        output.write_all(pack)?;
        Ok(())
    }
}
