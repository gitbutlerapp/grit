//! Serving fetches and clones (`upload-pack`).

use std::collections::{HashMap, HashSet, VecDeque};
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
use crate::pack_objects::{build_pack_for_upload, PackBuildOptions};
use crate::repo::Repository;
use crate::rev_list::ObjectFilter;

/// Capabilities offered in a v0/v1 advertisement, before the per-repository ones.
const V0_CAPABILITIES: &[&str] = &[
    "multi_ack_detailed",
    "side-band-64k",
    "thin-pack",
    "no-progress",
    "include-tag",
    "ofs-delta",
];

/// Serve one fetch session: advertise refs, negotiate, and send a pack.
pub fn upload_pack(
    repo: &Repository,
    input: &mut dyn Read,
    output: &mut dyn Write,
    opts: &ServeOptions,
) -> Result<()> {
    let config = ConfigSet::load(repo.environment(), Some(&repo.git_dir), true).unwrap_or_default();
    let policy = UploadPackPolicy::from_config(&config);
    let result = match opts.protocol {
        ProtocolVersion::V2 => serve_v2(repo, input, output, opts, &config, &policy),
        ProtocolVersion::V0 | ProtocolVersion::V1 => {
            serve_v0(repo, input, output, opts, &config, &policy)
        }
    };
    match result {
        Ok(()) => {
            output.flush()?;
            Ok(())
        }
        Err(e) => {
            if let Some(msg) = err_pkt_line(&e) {
                let _ = write_line(output, &format!("ERR {msg}"));
                let _ = output.flush();
                return Ok(());
            }
            Err(e)
        }
    }
}

fn err_pkt_line(e: &ServeError) -> Option<String> {
    match e {
        ServeError::NotOurRef(oid) => Some(format!("upload-pack: not our ref {}", oid.to_hex())),
        ServeError::UploadFilter(f) => Some(f.to_string()),
        ServeError::Protocol(msg) if msg.starts_with("filter") || msg.contains("unknown ref") => {
            Some(msg.clone())
        }
        _ => None,
    }
}

fn serve_v0(
    repo: &Repository,
    input: &mut dyn Read,
    output: &mut dyn Write,
    opts: &ServeOptions,
    config: &ConfigSet,
    policy: &UploadPackPolicy,
) -> Result<()> {
    let refs = advertised_refs(repo, &opts.hidden_refs)?;
    let head = head_info(repo);

    if opts.advertise_refs || !opts.stateless_rpc {
        if opts.protocol == ProtocolVersion::V1 {
            write_line(output, "version 1")?;
        }
        write_v0_advertisement(repo, output, &refs, &head, opts, policy)?;
        if opts.advertise_refs {
            return Ok(());
        }
        output.flush()?;
    }

    let Some(request) = read_v0_request(repo, input, config, policy)? else {
        return Ok(());
    };
    if request.filter.is_some() {
        validate_wants_filtered(repo, &request.wants)?;
    } else {
        validate_wants(repo, &refs, head.oid, &request.wants, policy)?;
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
    // Duplex transports read the shallow/unshallow lines before the have
    // exchange; stateless HTTP sends wants+done in one POST and expects them
    // after `done` (see `read_stateless_response_stream` / `git fetch-pack`).
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
    if opts.stateless_rpc {
        if let Some(ref sh) = shallow_resp {
            write_v0_shallow_lines(output, sh)?;
            write_flush(output)?;
        }
    }
    let sideband = request.caps.contains("side-band-64k");
    let pack = build_response_pack(
        repo,
        &refs,
        &request.wants,
        &common,
        &request.caps,
        request.filter.as_ref(),
        shallow_resp.as_ref(),
    )?;
    write_v0_pack(output, &pack, sideband)
}

fn write_v0_advertisement(
    repo: &Repository,
    output: &mut dyn Write,
    refs: &[AdvertisedRef],
    head: &super::HeadInfo,
    opts: &ServeOptions,
    policy: &UploadPackPolicy,
) -> Result<()> {
    let mut caps: Vec<String> = V0_CAPABILITIES.iter().map(|c| (*c).to_owned()).collect();
    caps.extend(
        policy
            .v0_capability_tokens()
            .iter()
            .map(|c| (*c).to_owned()),
    );
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
    write_shallow_lines(output, sh, false)
}

/// Write `shallow` / `unshallow` lines for upload-pack (v0 or v2 `shallow-info`).
fn write_shallow_lines(
    output: &mut dyn Write,
    sh: &ShallowResponse,
    sideband_all: bool,
) -> Result<()> {
    for oid in &sh.shallow {
        write_fetch_line(output, &format!("shallow {}", oid.to_hex()), sideband_all)?;
    }
    for oid in &sh.unshallow {
        write_fetch_line(output, &format!("unshallow {}", oid.to_hex()), sideband_all)?;
    }
    Ok(())
}

/// One v2 fetch response line; with `sideband-all`, payload is band 1 (mirrors `packet_writer_write`).
fn write_fetch_line(output: &mut dyn Write, line: &str, sideband_all: bool) -> Result<()> {
    if sideband_all {
        let mut payload = Vec::from(line.as_bytes());
        payload.push(b'\n');
        crate::pkt_line::write_sideband_packet(&mut &mut *output, 1, &payload)?;
    } else {
        write_line(output, line)?;
    }
    Ok(())
}

fn write_v2_wanted_refs(
    output: &mut dyn Write,
    wanted: &HashMap<String, ObjectId>,
    sideband_all: bool,
) -> Result<()> {
    if wanted.is_empty() {
        return Ok(());
    }
    write_fetch_line(output, "wanted-refs", sideband_all)?;
    for (name, oid) in wanted {
        write_fetch_line(output, &format!("{} {name}", oid.to_hex()), sideband_all)?;
    }
    write_delim(output)
}

fn write_delim(output: &mut dyn Write) -> Result<()> {
    output.write_all(crate::pkt_line::DELIM.as_bytes())?;
    Ok(())
}

fn serve_v2(
    repo: &Repository,
    input: &mut dyn Read,
    output: &mut dyn Write,
    opts: &ServeOptions,
    config: &ConfigSet,
    policy: &UploadPackPolicy,
) -> Result<()> {
    if opts.advertise_refs || !opts.stateless_rpc {
        write_line(output, "version 2")?;
        if !opts.agent.is_empty() {
            write_line(output, &format!("agent={}", opts.agent))?;
        }
        write_line(output, "ls-refs=unborn")?;
        let fetch_feats = policy.v2_fetch_features().join(" ");
        write_line(output, &format!("fetch={fetch_feats}"))?;
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
            "fetch" => fetch_v2(repo, output, &request.args, opts, config, policy)?,
            other => return Err(ServeError::UnknownCommand(other.to_owned())),
        }
        output.flush()?;
        if opts.stateless_rpc {
            return Ok(());
        }
    }
}

struct V2Request {
    command: String,
    object_format: Option<String>,
    args: Vec<String>,
}

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
    config: &ConfigSet,
    policy: &UploadPackPolicy,
) -> Result<()> {
    let refs = advertised_refs(repo, &opts.hidden_refs)?;
    let head = head_info(repo);
    let mut wants = Vec::new();
    let mut haves = Vec::new();
    let mut done = false;
    let mut caps = HashSet::new();
    let mut shallow = ShallowRequest::default();
    let mut filter: Option<ObjectFilter> = None;
    let mut wanted_refs: HashMap<String, ObjectId> = HashMap::new();

    for arg in args {
        if let Some(hex) = arg.strip_prefix("want ") {
            wants.push(parse_oid(repo, hex.trim())?);
        } else if let Some(hex) = arg.strip_prefix("have ") {
            haves.push(parse_oid(repo, hex.trim())?);
        } else if let Some(name) = arg.strip_prefix("want-ref ") {
            if !policy.ref_in_want {
                return Err(ServeError::Protocol("want-ref not supported".into()));
            }
            let oid = resolve_want_ref(repo, &refs, &opts.hidden_refs, name.trim())?;
            wanted_refs.insert(name.trim().to_owned(), oid);
            wants.push(oid);
        } else if arg == "done" {
            done = true;
        } else if matches!(
            arg.as_str(),
            "thin-pack" | "no-progress" | "include-tag" | "ofs-delta" | "sideband-all"
        ) {
            caps.insert(arg.clone());
        } else if parse_shallow_line(repo, arg, &mut shallow, config, policy, &mut filter)? {
            continue;
        } else {
            return Err(ServeError::Protocol(format!(
                "unsupported fetch argument '{arg}'"
            )));
        }
    }

    if filter.is_some() {
        validate_wants_filtered(repo, &wants)?;
    } else {
        validate_wants(repo, &refs, head.oid, &wants, policy)?;
    }

    let shallow_resp = if shallow.is_active() {
        Some(compute_shallow_response(repo, &wants, &shallow)?)
    } else {
        None
    };

    let mut seen = HashSet::new();
    let common: Vec<ObjectId> = haves
        .into_iter()
        .filter(|h| seen.insert(*h) && repo.odb.exists(h))
        .collect();

    let sideband_all = caps.contains("sideband-all");

    if !done {
        write_fetch_line(output, "acknowledgments", sideband_all)?;
        if common.is_empty() {
            write_fetch_line(output, "NAK", sideband_all)?;
        }
        for oid in &common {
            write_fetch_line(output, &format!("ACK {}", oid.to_hex()), sideband_all)?;
        }
        return write_flush(output);
    }

    if shallow.is_active() {
        write_fetch_line(output, "shallow-info", sideband_all)?;
        if let Some(sh) = shallow_resp.as_ref() {
            write_shallow_lines(output, sh, sideband_all)?;
        }
        write_delim(output)?;
    }
    write_v2_wanted_refs(output, &wanted_refs, sideband_all)?;

    let pack = build_response_pack(
        repo,
        &refs,
        &wants,
        &common,
        &caps,
        filter.as_ref(),
        shallow_resp.as_ref(),
    )?;
    write_fetch_line(output, "packfile", sideband_all)?;
    write_v2_packfile_body(output, &pack)
}

fn build_response_pack(
    repo: &Repository,
    refs: &[AdvertisedRef],
    wants: &[ObjectId],
    common: &[ObjectId],
    caps: &HashSet<String>,
    filter: Option<&ObjectFilter>,
    shallow: Option<&ShallowResponse>,
) -> Result<Vec<u8>> {
    let mut pack_wants = wants.to_vec();
    if let Some(sh) = shallow {
        pack_wants.extend(sh.extra_wants.iter().copied());
    }
    if caps.contains("include-tag") {
        pack_wants.extend(included_tags(repo, refs, wants, common)?);
    }
    pack_wants.sort();
    pack_wants.dedup();

    // Client `have` lines include shallow commits being deepened; their missing
    // parents must still be packed (Git unshallow). Do not treat those haves as
    // common bases for object exclusion.
    let pack_haves: Vec<ObjectId> = if let Some(sh) = shallow {
        common
            .iter()
            .filter(|oid| !sh.unshallow.contains(oid))
            .copied()
            .collect()
    } else {
        common.to_vec()
    };

    let pack_opts = PackBuildOptions {
        thin: caps.contains("thin-pack"),
        delta: true,
        use_ofs_delta: caps.contains("ofs-delta"),
        ..PackBuildOptions::default()
    };
    let shallow_grafts = shallow
        .map(|s| s.pack_shallow_grafts.clone())
        .unwrap_or_default();
    Ok(build_pack_for_upload(
        repo,
        &pack_wants,
        &pack_haves,
        &shallow_grafts,
        filter,
        &pack_opts,
    )?)
}

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

fn write_v0_pack(output: &mut dyn Write, pack: &[u8], side_band_64k: bool) -> Result<()> {
    if side_band_64k {
        crate::pkt_line::write_sideband_channel1_64k(&mut &mut *output, pack)?;
        write_flush(output)
    } else {
        output.write_all(pack)?;
        Ok(())
    }
}

/// Protocol v2 `packfile` section body is always side-band-64k framed on band 1,
/// independent of the `sideband-all` capability (which only affects other sections).
fn write_v2_packfile_body(output: &mut dyn Write, pack: &[u8]) -> Result<()> {
    crate::pkt_line::write_sideband_channel1_64k(&mut &mut *output, pack)?;
    write_flush(output)
}
