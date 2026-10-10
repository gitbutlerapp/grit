//! Serving fetches and clones (`upload-pack`).

use std::collections::{HashSet, VecDeque};
use std::io::{Read, Write};

use super::{
    advertised_refs, check_object_format, head_info, parse_oid, read_packet, write_flush,
    write_line, write_v2_fetch_line, AdvertisedRef, Packet, ProtocolVersion, Result, ServeError,
    ServeOptions,
};
use crate::config::ConfigSet;
use crate::objects::{ObjectId, ObjectKind};
use crate::pack_objects::{write_upload_pack_stream, PackBuildOptions, PackProgress};
use crate::repo::Repository;
use crate::serve::shallow_upload::{compute_shallow_response, ShallowRequest};
use crate::serve::upload_pack_policy::{resolve_want_ref, UploadPackPolicy};
use crate::shallow::INFINITE_DEPTH;

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
/// Clients may only ask for objects the advertisement offered (ref tips and the
/// objects annotated tags peel to). Shallow fetches, partial-clone filters and
/// `want-ref` are not offered, so clients do not request them.
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
    output.flush()?;
    result
}

fn serve_v0(
    repo: &Repository,
    input: &mut dyn Read,
    output: &mut dyn Write,
    opts: &ServeOptions,
) -> Result<()> {
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

    let Some(request) = read_v0_wants(repo, input)? else {
        // The client only wanted the advertisement (for example `ls-remote`).
        return Ok(());
    };
    let allowed = allowed_wants(&refs, head.oid);
    if let Some(bad) = request.wants.iter().find(|w| !allowed.contains(w)) {
        return Err(ServeError::NotOurRef(*bad));
    }

    let mut stash: Option<Packet> = None;
    if opts.stateless_rpc && request.is_shallow() {
        let next = read_packet_stash(input, &mut stash)?;
        if next.is_none() {
            let shallow_req = ShallowRequest {
                client_shallow: request.client_shallow.clone(),
                depth: request.deepen,
                deepen_since: None,
                deepen_not: Vec::new(),
                deepen_relative: request.deepen_relative,
            };
            let shallow_resp = compute_shallow_response(repo, &request.wants, &shallow_req)?;
            if request.deepen == Some(INFINITE_DEPTH) {
                write_v0_unshallow_info(output, &shallow_resp.unshallow)?;
            } else {
                write_v0_shallow_info(output, &shallow_resp.shallow)?;
                for oid in &shallow_resp.unshallow {
                    write_line(output, &format!("unshallow {}", oid.to_hex()))?;
                }
                write_flush(output)?;
            }
            return Ok(());
        }
        if let Some(pkt) = next {
            stash = Some(pkt);
        }
    }

    let detailed = request.caps.contains("multi_ack_detailed");
    let mut common: Vec<ObjectId> = Vec::new();
    let mut seen: HashSet<ObjectId> = HashSet::new();
    loop {
        match read_packet_stash(input, &mut stash)? {
            // A stateless request ends without `done`, or the client hung up.
            None => return Ok(()),
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
                // Stateless smart HTTP replays the full want/have transcript in each
                // POST; flushes separate rounds but `done` may follow later in the
                // same body. End without a pack only when the request body hits EOF.
            }
            Some(other) => {
                return Err(ServeError::Protocol(format!(
                    "unexpected packet during negotiation: {other:?}"
                )))
            }
        }
    }

    match common.last() {
        Some(last) if detailed => write_line(output, &format!("ACK {}", last.to_hex()))?,
        Some(_) => {}
        None => write_line(output, "NAK")?,
    }
    let sideband = request.caps.contains("side-band-64k");
    let shallow_req = ShallowRequest {
        client_shallow: request.client_shallow.clone(),
        depth: request.deepen,
        deepen_since: None,
        deepen_not: Vec::new(),
        deepen_relative: request.deepen_relative,
    };
    let shallow_resp = if request.is_shallow() {
        compute_shallow_response(repo, &request.wants, &shallow_req)?
    } else {
        Default::default()
    };
    if request.is_shallow() {
        if request.deepen == Some(INFINITE_DEPTH) {
            write_v0_unshallow_info(output, &shallow_resp.unshallow)?;
        } else {
            write_v0_shallow_info(output, &shallow_resp.shallow)?;
            for oid in &shallow_resp.unshallow {
                write_line(output, &format!("unshallow {}", oid.to_hex()))?;
            }
            write_flush(output)?;
        }
        output.flush()?;
    }
    let mut pack_wants = request.wants.clone();
    pack_wants.extend(shallow_resp.extra_wants.clone());
    stream_response_pack(
        repo,
        &refs,
        &pack_wants,
        &common,
        &request.caps,
        output,
        sideband,
        &shallow_resp.pack_shallow_grafts,
        &shallow_resp.have_shallow_grafts,
    )
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
    let mut caps: Vec<String> = V0_CAPABILITIES.iter().map(|c| (*c).to_owned()).collect();
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

/// The want block of a v0 request.
struct V0Request {
    wants: Vec<ObjectId>,
    caps: HashSet<String>,
    client_shallow: Vec<ObjectId>,
    deepen: Option<u32>,
    deepen_relative: bool,
}

impl V0Request {
    fn is_shallow(&self) -> bool {
        self.deepen.is_some() || !self.client_shallow.is_empty()
    }
}

/// Read `want` lines up to the flush. Returns `None` when the client sent no
/// wants at all (a bare flush or end of input).
fn read_v0_wants(repo: &Repository, input: &mut dyn Read) -> Result<Option<V0Request>> {
    let mut wants = Vec::new();
    let mut caps = HashSet::new();
    let mut client_shallow = Vec::new();
    let mut deepen = None;
    let mut deepen_relative = false;
    loop {
        match read_packet(input)? {
            None | Some(Packet::Flush) => break,
            Some(Packet::Data(line)) => {
                let line = line.trim_end();
                if let Some(rest) = line.strip_prefix("want ") {
                    let mut parts = rest.split(' ');
                    let hex = parts.next().unwrap_or_default();
                    wants.push(parse_oid(repo, hex)?);
                    if wants.len() == 1 {
                        for cap in parts.filter(|c| !c.is_empty()) {
                            if cap == "deepen-relative" {
                                deepen_relative = true;
                            } else {
                                caps.insert(cap.to_owned());
                            }
                        }
                    }
                } else if let Some(rest) = line.strip_prefix("shallow ") {
                    client_shallow.push(parse_oid(repo, rest.trim())?);
                } else if let Some(rest) = line.strip_prefix("deepen ") {
                    let n: u32 = rest
                        .trim()
                        .parse()
                        .map_err(|_| ServeError::Protocol(format!("bad deepen '{line}'")))?;
                    deepen = Some(n);
                } else if line.starts_with("deepen-since ") || line.starts_with("deepen-not ") {
                    return Err(ServeError::Protocol(format!(
                        "unsupported shallow extension '{line}'"
                    )));
                } else {
                    return Err(ServeError::Protocol(format!(
                        "unsupported request line '{line}'"
                    )));
                }
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
        client_shallow,
        deepen,
        deepen_relative,
    }))
}

/// Objects a client may name in `want`: advertised tips and peeled tag targets.
fn allowed_wants(refs: &[AdvertisedRef], head: Option<ObjectId>) -> HashSet<ObjectId> {
    refs.iter()
        .flat_map(|r| std::iter::once(r.oid).chain(r.peeled))
        .chain(head)
        .collect()
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
        let config =
            ConfigSet::load(repo.environment(), Some(&repo.git_dir), true).unwrap_or_default();
        let policy = UploadPackPolicy::from_config(&config);
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
    let mut wants = Vec::new();
    let mut haves = Vec::new();
    let mut done = false;
    let mut caps = HashSet::new();
    let mut deepen = None;
    let mut client_shallow = Vec::new();
    let mut deepen_relative = false;
    let config = ConfigSet::load(repo.environment(), Some(&repo.git_dir), true).unwrap_or_default();
    let policy = UploadPackPolicy::from_config(&config);
    let mut filter_spec: Option<String> = None;
    let mut want_ref_names: Vec<String> = Vec::new();
    for arg in args {
        if let Some(hex) = arg.strip_prefix("want ") {
            wants.push(parse_oid(repo, hex.trim())?);
        } else if let Some(hex) = arg.strip_prefix("have ") {
            haves.push(parse_oid(repo, hex.trim())?);
        } else if let Some(rest) = arg.strip_prefix("shallow ") {
            client_shallow.push(parse_oid(repo, rest.trim())?);
        } else if let Some(spec) = arg.strip_prefix("filter ") {
            filter_spec = Some(spec.trim().to_owned());
        } else if let Some(rest) = arg.strip_prefix("deepen ") {
            deepen = Some(
                rest.trim()
                    .parse()
                    .map_err(|_| ServeError::Protocol(format!("bad deepen '{arg}'")))?,
            );
        } else if arg == "done" {
            done = true;
        } else if matches!(
            arg.as_str(),
            "thin-pack"
                | "no-progress"
                | "include-tag"
                | "ofs-delta"
                | "sideband-all"
                | "deepen-relative"
        ) {
            if arg == "deepen-relative" {
                deepen_relative = true;
            } else {
                caps.insert(arg.clone());
            }
        } else if let Some(name) = arg.strip_prefix("want-ref ") {
            want_ref_names.push(name.trim().to_owned());
        } else if arg.starts_with("deepen-since ") || arg.starts_with("deepen-not ") {
            return Err(ServeError::Protocol(format!(
                "unsupported shallow extension '{arg}'"
            )));
        } else {
            return Err(ServeError::Protocol(format!(
                "unsupported fetch argument '{arg}'"
            )));
        }
    }

    let refs = advertised_refs(repo, &opts.hidden_refs)?;
    if !want_ref_names.is_empty() {
        if !policy.ref_in_want {
            return Err(ServeError::Protocol(
                "want-ref not supported (uploadpack.allowRefInWant is false)".into(),
            ));
        }
        for name in want_ref_names {
            wants.push(resolve_want_ref(repo, &refs, &opts.hidden_refs, &name)?);
        }
    }

    if let Some(spec) = filter_spec {
        if !policy.allow_filter {
            write_line(output, "ERR filter not allowed by uploadpack.allowFilter")?;
            return write_flush(output);
        }
        policy.parse_filter(&config, &spec)?;
    }

    let allowed = allowed_wants(&refs, head_info(repo).oid);
    if let Some(bad) = wants.iter().find(|w| !allowed.contains(w)) {
        return Err(ServeError::NotOurRef(*bad));
    }

    let mut seen = HashSet::new();
    let common: Vec<ObjectId> = haves
        .into_iter()
        .filter(|h| seen.insert(*h) && repo.odb.exists(h))
        .collect();

    let sideband_all = caps.contains("sideband-all");

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

    let shallow_req = ShallowRequest {
        client_shallow,
        depth: deepen,
        deepen_since: None,
        deepen_not: Vec::new(),
        deepen_relative,
    };
    let shallow_active = shallow_req.is_active();
    let shallow_resp = if shallow_active {
        compute_shallow_response(repo, &wants, &shallow_req)?
    } else {
        Default::default()
    };
    if shallow_active {
        if deepen == Some(INFINITE_DEPTH) {
            write_v2_unshallow_info(output, &shallow_resp.unshallow, sideband_all)?;
        } else {
            write_v2_shallow_update(output, &shallow_resp, sideband_all)?;
        }
    }
    write_v2_fetch_line(output, "packfile", sideband_all)?;
    if shallow_active {
        let mut pack_wants = wants.clone();
        pack_wants.extend(shallow_resp.extra_wants.clone());
        stream_response_pack(
            repo,
            &refs,
            &pack_wants,
            &common,
            &caps,
            output,
            true,
            &shallow_resp.pack_shallow_grafts,
            &shallow_resp.have_shallow_grafts,
        )
    } else {
        stream_response_pack(
            repo,
            &refs,
            &wants,
            &common,
            &caps,
            output,
            true,
            &HashSet::new(),
            &HashSet::new(),
        )
    }
}

fn read_packet_stash(input: &mut dyn Read, stash: &mut Option<Packet>) -> Result<Option<Packet>> {
    if let Some(pkt) = stash.take() {
        return Ok(Some(pkt));
    }
    read_packet(input)
}

fn write_v0_shallow_info(output: &mut dyn Write, boundaries: &[ObjectId]) -> Result<()> {
    for oid in boundaries {
        write_line(output, &format!("shallow {}", oid.to_hex()))?;
    }
    write_flush(output)
}

fn write_v0_unshallow_info(output: &mut dyn Write, grafts: &[ObjectId]) -> Result<()> {
    for oid in grafts {
        write_line(output, &format!("unshallow {}", oid.to_hex()))?;
    }
    write_flush(output)
}

fn write_v2_shallow_update(
    output: &mut dyn Write,
    update: &crate::serve::shallow_upload::ShallowResponse,
    sideband_all: bool,
) -> Result<()> {
    write_v2_fetch_line(output, "shallow-info", sideband_all)?;
    for oid in &update.shallow {
        write_v2_fetch_line(output, &format!("shallow {}", oid.to_hex()), sideband_all)?;
    }
    for oid in &update.unshallow {
        write_v2_fetch_line(output, &format!("unshallow {}", oid.to_hex()), sideband_all)?;
    }
    write_v2_section_delim(output)
}

fn write_v2_unshallow_info(
    output: &mut dyn Write,
    grafts: &[ObjectId],
    sideband_all: bool,
) -> Result<()> {
    write_v2_fetch_line(output, "shallow-info", sideband_all)?;
    for oid in grafts {
        write_v2_fetch_line(output, &format!("unshallow {}", oid.to_hex()), sideband_all)?;
    }
    write_v2_section_delim(output)
}

fn write_v2_section_delim(output: &mut dyn Write) -> Result<()> {
    write!(output, "0001").map_err(crate::error::Error::Io)?;
    Ok(())
}

/// Stream the pack answering a fetch, adding annotated tags when `include-tag`
/// was negotiated.
fn stream_response_pack(
    repo: &Repository,
    refs: &[AdvertisedRef],
    wants: &[ObjectId],
    common: &[ObjectId],
    caps: &HashSet<String>,
    output: &mut dyn Write,
    sideband: bool,
    source_shallow: &HashSet<ObjectId>,
    have_shallow: &HashSet<ObjectId>,
) -> Result<()> {
    let mut pack_wants = wants.to_vec();
    if caps.contains("include-tag") {
        pack_wants.extend(included_tags(repo, refs, wants, common)?);
    }
    pack_wants.sort();
    pack_wants.dedup();

    let pack_opts = PackBuildOptions {
        thin: caps.contains("thin-pack"),
        delta: true,
        use_ofs_delta: caps.contains("ofs-delta"),
        ..PackBuildOptions::default()
    };
    let mut progress = |_ev: PackProgress| {};
    if sideband {
        let mut framed = crate::pkt_line::SidebandChannel1Writer::new(&mut *output);
        write_upload_pack_stream(
            repo,
            &pack_wants,
            common,
            source_shallow,
            have_shallow,
            None,
            &pack_opts,
            &mut progress,
            &mut framed,
        )
        .map_err(ServeError::from)?;
        framed.finish().map_err(ServeError::from)?;
        write_flush(output)?;
        output.flush().map_err(ServeError::from)
    } else {
        write_upload_pack_stream(
            repo,
            &pack_wants,
            common,
            source_shallow,
            have_shallow,
            None,
            &pack_opts,
            &mut progress,
            output,
        )
        .map_err(ServeError::from)?;
        Ok(())
    }
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
