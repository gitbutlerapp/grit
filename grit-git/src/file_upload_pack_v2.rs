//! Protocol v2 over local `grit upload-pack` for `file://` URLs (tests, `ls-remote`, clone).

use std::io::{Cursor, Read, Write};
use std::net::Shutdown;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};
use grit_lib::config::ConfigSet;
use grit_lib::objects::ObjectId;
use grit_lib::repo::Repository;

use crate::grit_exe::{grit_executable, strip_trace2_env};
use crate::wire_trace;
use grit_lib::pkt_line;

fn trace_packet_git(direction: char, payload: &str) {
    let identity = crate::trace_packet::negotiation_packet_label();
    if identity == "clone" && direction == '>' && payload.starts_with("want ") {
        return;
    }
    wire_trace::trace_packet_line_ident(identity, direction, payload);
}

/// True when `protocol.version` from config resolves to 2 (Git `-c protocol.version=2`).
pub(crate) fn client_wants_protocol_v2() -> bool {
    crate::protocol_wire::effective_client_protocol_version() == 2
}

/// `transfer.bundleURI` default-on matches Git; explicit `false` disables the bundle-uri command.
pub(crate) fn transfer_bundle_uri_enabled() -> bool {
    let set = ConfigSet::load(None, true).unwrap_or_default();
    match set.get_bool("transfer.bundleuri") {
        Some(Ok(b)) => b,
        Some(Err(_)) => true,
        None => true,
    }
}

fn spawn_upload_pack_readonly(
    cmd_template: Option<&str>,
    repo_path: &Path,
) -> Result<std::process::Child> {
    let repo_path = repo_path
        .canonicalize()
        .unwrap_or_else(|_| repo_path.to_path_buf());
    let rp = repo_path.to_string_lossy();
    let rp_escaped = rp.replace('\'', "'\"'\"'");

    let base = |c: &mut Command| {
        strip_trace2_env(c);
        c.env("GIT_PROTOCOL", "version=2")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
    };

    let Some(cmd_template) = cmd_template else {
        let mut c = Command::new(grit_executable());
        base(&mut c);
        c.arg("upload-pack").arg(rp.as_ref());
        return c
            .spawn()
            .with_context(|| format!("failed to spawn grit upload-pack for {}", rp));
    };

    let (leading_env, after_env) =
        crate::fetch_transport::parse_leading_shell_env_assignments(cmd_template);
    if after_env.contains("git-upload-pack") {
        let mut c = Command::new(grit_executable());
        base(&mut c);
        for (k, v) in leading_env {
            c.env(k, v);
        }
        c.arg("upload-pack").arg(rp.as_ref());
        return c
            .spawn()
            .with_context(|| format!("failed to spawn grit upload-pack for {}", rp));
    }

    let trimmed = cmd_template.trim();
    if trimmed == "grit-upload-pack" || trimmed.ends_with("/grit-upload-pack") {
        let mut c = Command::new(trimmed);
        base(&mut c);
        c.arg(rp.as_ref());
        return c
            .spawn()
            .with_context(|| format!("failed to spawn '{} {}'", trimmed, rp));
    }

    let full_cmd = cmd_template.replace('\'', "'\"'\"'");
    let script = format!("{full_cmd} '{rp_escaped}'");
    let mut c = Command::new("sh");
    base(&mut c);
    c.arg("-c").arg(&script);
    c.spawn()
        .with_context(|| format!("failed to spawn upload-pack: {script}"))
}

/// Read pkt-lines from `r`, appending raw wire bytes to `out`, until a flush packet (`0000`).
pub(crate) fn read_pkt_lines_until_flush(
    r: &mut impl Read,
    out: &mut Vec<u8>,
    max_total: usize,
) -> Result<()> {
    let mut total = 0usize;
    loop {
        let mut len_buf = [0u8; 4];
        r.read_exact(&mut len_buf)
            .map_err(|e| anyhow::Error::from(e))?;
        total += 4;
        if total > max_total {
            bail!("v2 response exceeds size limit");
        }
        out.extend_from_slice(&len_buf);
        let len_str = std::str::from_utf8(&len_buf)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        let n = usize::from_str_radix(len_str, 16)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        match n {
            0 => return Ok(()),
            1 | 2 => {
                bail!("unexpected special pkt-line in ls-refs response");
            }
            n if n <= 4 => {
                bail!("invalid pkt-line length: {n}");
            }
            n => {
                let payload_len = n - 4;
                total += payload_len;
                if total > max_total {
                    bail!("v2 response exceeds size limit");
                }
                let mut payload = vec![0u8; payload_len];
                r.read_exact(&mut payload)
                    .map_err(|e| anyhow::Error::from(e))?;
                out.extend_from_slice(&payload);
            }
        }
    }
}

pub(crate) fn read_v2_capability_block(stdout: &mut impl Read) -> Result<Vec<String>> {
    let mut caps = Vec::new();
    loop {
        let pkt = pkt_line::read_packet(stdout).context("read v2 capability pkt-line")?;
        match pkt {
            None => bail!("unexpected EOF in v2 capability advertisement"),
            Some(pkt_line::Packet::Flush) => break,
            Some(pkt_line::Packet::Data(line)) => {
                trace_packet_git('<', &line);
                caps.push(line);
            }
            Some(other) => bail!("unexpected packet in v2 caps: {other:?}"),
        }
    }
    Ok(caps)
}

pub(crate) use grit_lib::protocol_v2::server_advertises_bundle_uri;

pub(crate) use grit_lib::protocol_v2::cap_lines_for_command_request as cap_lines_for_bundle_request;

pub(crate) fn write_bundle_uri_command(stdin: &mut impl Write, cap_send: &[String]) -> Result<()> {
    trace_packet_git('>', "command=bundle-uri");
    pkt_line::write_line(stdin, "command=bundle-uri")?;
    for line in cap_send {
        trace_packet_git('>', line);
        pkt_line::write_line(stdin, line)?;
    }
    pkt_line::write_delim(stdin)?;
    trace_packet_git('>', "0001");
    pkt_line::write_flush(stdin)?;
    trace_packet_git('>', "0000");
    stdin.flush()?;
    Ok(())
}

pub(crate) fn drain_bundle_uri_response(stdout: &mut impl Read) -> Result<()> {
    loop {
        match pkt_line::read_packet(stdout).context("read bundle-uri response")? {
            None => break,
            Some(pkt_line::Packet::Flush) => break,
            Some(pkt_line::Packet::Data(line)) => {
                trace_packet_git('<', &line);
            }
            Some(other) => bail!("unexpected bundle-uri packet: {other:?}"),
        }
    }
    Ok(())
}

fn write_ls_refs_for_clone(stdin: &mut impl Write, object_format: &str) -> Result<()> {
    trace_packet_git('>', "command=ls-refs");
    pkt_line::write_line(stdin, "command=ls-refs")?;
    let agent = format!("agent=git/{}-", crate::version_string());
    trace_packet_git('>', agent.trim_end());
    pkt_line::write_line(stdin, &agent)?;
    let of = format!("object-format={object_format}");
    trace_packet_git('>', &of);
    pkt_line::write_line(stdin, &of)?;
    pkt_line::write_delim(stdin)?;
    trace_packet_git('>', "0001");
    trace_packet_git('>', "symrefs");
    pkt_line::write_line(stdin, "symrefs")?;
    trace_packet_git('>', "peel");
    pkt_line::write_line(stdin, "peel")?;
    trace_packet_git('>', "unborn");
    pkt_line::write_line(stdin, "unborn")?;
    trace_packet_git('>', "ref-prefix HEAD");
    pkt_line::write_line(stdin, "ref-prefix HEAD")?;
    trace_packet_git('>', "ref-prefix refs/heads/");
    pkt_line::write_line(stdin, "ref-prefix refs/heads/")?;
    trace_packet_git('>', "ref-prefix refs/tags/");
    pkt_line::write_line(stdin, "ref-prefix refs/tags/")?;
    pkt_line::write_flush(stdin)?;
    trace_packet_git('>', "0000");
    stdin.flush()?;
    Ok(())
}

fn write_ls_refs_request_for_ls_remote(
    stdin: &mut impl Write,
    object_format: &str,
    args: &crate::commands::ls_remote::Args,
    server_options: &[String],
) -> Result<()> {
    pkt_line::write_line(stdin, "command=ls-refs")?;
    trace_packet_git('>', "command=ls-refs");
    let agent = format!("agent=git/{}-", crate::version_string());
    pkt_line::write_line(stdin, &agent)?;
    trace_packet_git('>', agent.trim_end());
    let of = format!("object-format={object_format}");
    pkt_line::write_line(stdin, &of)?;
    trace_packet_git('>', &of);
    for opt in server_options {
        let line = format!("server-option={opt}");
        pkt_line::write_line(stdin, &line)?;
        trace_packet_git('>', &line);
    }
    pkt_line::write_delim(stdin)?;
    trace_packet_git('>', "0001");
    if args.symref {
        pkt_line::write_line(stdin, "symrefs")?;
        trace_packet_git('>', "symrefs");
    }
    if !args.refs_only {
        pkt_line::write_line(stdin, "peel")?;
        trace_packet_git('>', "peel");
    }
    if args.branches {
        pkt_line::write_line(stdin, "ref-prefix refs/heads/")?;
        trace_packet_git('>', "ref-prefix refs/heads/");
    }
    if args.tags {
        pkt_line::write_line(stdin, "ref-prefix refs/tags/")?;
        trace_packet_git('>', "ref-prefix refs/tags/");
    }
    pkt_line::write_flush(stdin)?;
    trace_packet_git('>', "0000");
    stdin.flush()?;
    Ok(())
}

fn skip_ls_refs_response(stdout: &mut impl Read) -> Result<()> {
    loop {
        match pkt_line::read_packet(stdout)? {
            None => break,
            Some(pkt_line::Packet::Flush) => break,
            Some(pkt_line::Packet::Data(line)) => {
                trace_packet_git('<', &line);
            }
            Some(other) => bail!("unexpected ls-refs packet: {other:?}"),
        }
    }
    Ok(())
}

fn source_head_oid_from_repo_head_file(source_git_dir: &Path) -> Option<String> {
    let head_path = source_git_dir.join("HEAD");
    let content = std::fs::read_to_string(head_path).ok()?;
    let trimmed = content.trim();
    if trimmed.len() == 40 && trimmed.chars().all(|c| c.is_ascii_hexdigit()) {
        return Some(trimmed.to_owned());
    }
    None
}

fn collect_clone_ls_refs_metadata(
    buf: &[u8],
) -> Result<(Vec<ObjectId>, Option<String>, Option<String>)> {
    let mut cursor = Cursor::new(buf);
    let mut wants: Vec<ObjectId> = Vec::new();
    let mut head_symref: Option<String> = None;
    let mut head_oid: Option<String> = None;
    loop {
        let pkt = match pkt_line::read_packet(&mut cursor)? {
            None => break,
            Some(pkt_line::Packet::Flush) => break,
            Some(pkt_line::Packet::Data(line)) => line,
            Some(other) => bail!("unexpected ls-refs data: {other:?}"),
        };
        trace_packet_git('<', &pkt);
        let (name, oid, _peeled, symref_target) =
            crate::commands::ls_remote::parse_ls_refs_v2_line(&pkt)?;
        let name = name.trim().to_owned();
        if name == "HEAD" {
            // An unborn HEAD is advertised with the null OID (`unborn` token); it names no object,
            // only a symref target, so leave `head_oid` unset and let the symref drive checkout.
            if !oid.is_zero() {
                head_oid = Some(oid.to_hex());
            }
            if let Some(target) = symref_target {
                head_symref = Some(target);
            }
            continue;
        }
        if !oid.is_zero() && (name.starts_with("refs/heads/") || name.starts_with("refs/tags/")) {
            wants.push(oid);
        }
    }
    wants.sort_by_key(|o| o.to_hex());
    wants.dedup();
    Ok((wants, head_symref, head_oid))
}

fn source_head_symref_from_repo_head_file(source_git_dir: &Path) -> Option<String> {
    let head_path = source_git_dir.join("HEAD");
    let content = std::fs::read_to_string(head_path).ok()?;
    content
        .trim()
        .strip_prefix("ref: ")
        .map(|s| s.trim().to_owned())
}

fn should_use_source_head_symref_fallback(source_git_dir: &Path) -> bool {
    let set = ConfigSet::load(Some(source_git_dir), true).unwrap_or_default();
    let unborn = set
        .get("lsrefs.unborn")
        .unwrap_or_else(|| "advertise".to_owned());
    !unborn.eq_ignore_ascii_case("ignore")
}

pub(crate) use grit_lib::protocol_v2::fetch_supports_sideband_all as v2_fetch_supports_sideband_all;

pub(crate) use grit_lib::protocol_v2::fetch_supports_ref_in_want as v2_fetch_supports_ref_in_want;

pub(crate) use grit_lib::protocol_v2::fetch_supports_filter as v2_fetch_supports_filter;

#[allow(clippy::too_many_arguments)]
pub(crate) fn write_v2_fetch_request(
    stdin: &mut impl Write,
    object_format: &str,
    wants: &[ObjectId],
    haves: &[ObjectId],
    sideband_all: bool,
    include_tag: bool,
    deepen_relative: bool,
    session_id_on_wire: Option<&str>,
    server_options: &[String],
    filter_spec: Option<&str>,
    shallow_oids: &[ObjectId],
    depth: Option<usize>,
    shallow_since: Option<&str>,
    shallow_exclude: &[String],
    unshallow: bool,
    promisor_remote_reply: Option<&str>,
    want_refs: &[String],
    send_done: bool,
) -> Result<()> {
    trace_packet_git('>', "command=fetch");
    pkt_line::write_line(stdin, "command=fetch")?;
    let agent = format!("agent=git/{}-", crate::version_string());
    trace_packet_git('>', agent.trim_end());
    pkt_line::write_line(stdin, &agent)?;
    let of = format!("object-format={object_format}");
    trace_packet_git('>', &of);
    pkt_line::write_line(stdin, &of)?;
    // `session-id` is a v2 capability (serve.c lists it with a `.receive` handler), so it belongs
    // in the request's capability list — before the `0001` delimiter — alongside agent and
    // object-format, not among the per-command fetch arguments (`t5705`).
    if let Some(sid) = session_id_on_wire {
        let esc = crate::trace2_transfer::json_escape_trace_value(sid);
        let line = format!("session-id={esc}");
        trace_packet_git('>', &line);
        pkt_line::write_line(stdin, &line)?;
    }
    for opt in server_options {
        let line = format!("server-option={opt}");
        trace_packet_git('>', &line);
        pkt_line::write_line(stdin, &line)?;
    }
    // `promisor-remote` is a v2 capability (connect.c `send_capabilities`): the client's accepted
    // reply belongs in the request capability list, before the `0001` delimiter (`t5710`).
    if let Some(reply) = promisor_remote_reply.filter(|r| !r.is_empty()) {
        let line = format!("promisor-remote={reply}");
        trace_packet_git('>', &line);
        pkt_line::write_line(stdin, &line)?;
    }
    pkt_line::write_delim(stdin)?;
    trace_packet_git('>', "0001");

    trace_packet_git('>', "thin-pack");
    pkt_line::write_line(stdin, "thin-pack")?;
    trace_packet_git('>', "no-progress");
    pkt_line::write_line(stdin, "no-progress")?;
    trace_packet_git('>', "ofs-delta");
    pkt_line::write_line(stdin, "ofs-delta")?;
    if sideband_all {
        trace_packet_git('>', "sideband-all");
        pkt_line::write_line(stdin, "sideband-all")?;
    }
    if include_tag {
        trace_packet_git('>', "include-tag");
        pkt_line::write_line(stdin, "include-tag")?;
    }

    for w in wants {
        let line = format!("want {}", w.to_hex());
        trace_packet_git('>', line.trim_end());
        wire_trace::trace_packet_upload_pack('<', line.trim_end());
        pkt_line::write_line(stdin, &line)?;
    }
    // Refs requested by name (not exact OID) go out as `want-ref <refname>` when the server
    // advertised the `ref-in-want` feature (matches `fetch-pack.c` `add_wants`). The server resolves
    // them in a `wanted-refs` section in its response.
    for r in want_refs {
        let line = format!("want-ref {r}");
        trace_packet_git('>', line.trim_end());
        wire_trace::trace_packet_upload_pack('<', line.trim_end());
        pkt_line::write_line(stdin, &line)?;
    }
    if let Some(spec) = filter_spec.map(str::trim).filter(|s| !s.is_empty()) {
        // Send the canonical/expanded filter spec on the wire, matching Git's
        // `expand_list_objects_filter_spec` (e.g. `blob:limit=1k` -> `blob:limit=1024`).
        let expanded = grit_lib::rev_list::expand_object_filter_for_protocol(spec)
            .unwrap_or_else(|_| spec.to_owned());
        let line = format!("filter {expanded}");
        trace_packet_git('>', &line);
        pkt_line::write_line(stdin, &line)?;
    }
    if deepen_relative {
        trace_packet_git('>', "deepen-relative");
        pkt_line::write_line(stdin, "deepen-relative")?;
    }
    for oid in shallow_oids {
        let line = format!("shallow {}", oid.to_hex());
        trace_packet_git('>', &line);
        pkt_line::write_line(stdin, &line)?;
    }
    if unshallow {
        trace_packet_git('>', "deepen 2147483647");
        pkt_line::write_line(stdin, "deepen 2147483647")?;
    } else if let Some(depth) = depth.filter(|d| *d > 0) {
        let line = format!("deepen {depth}");
        trace_packet_git('>', &line);
        pkt_line::write_line(stdin, &line)?;
    }
    if let Some(since) = shallow_since {
        // Send the Unix timestamp `approxidate` produces, not the raw date: `upload-pack` parses
        // `deepen-since` with `parse_timestamp` and rejects trailing text (t5539 fetch shallow since).
        let value = grit_lib::git_date::approx::approxidate_careful(since.trim(), None).to_string();
        let line = format!("deepen-since {value}");
        trace_packet_git('>', &line);
        pkt_line::write_line(stdin, &line)?;
    }
    for exclude in shallow_exclude {
        let line = format!("deepen-not {exclude}");
        trace_packet_git('>', &line);
        pkt_line::write_line(stdin, &line)?;
    }
    // Send the locally-available tips as `have` lines so the server can build a thin pack and so a
    // bundle-uri clone advertises the bundle tips during negotiation (t5558 `negotiation:` cases).
    // We send them all in this single round and immediately follow with `done`, so the server skips
    // the `acknowledgments` exchange and streams the pack directly.
    for h in haves {
        let line = format!("have {}", h.to_hex());
        trace_packet_git('>', line.trim_end());
        wire_trace::trace_packet_upload_pack('<', line.trim_end());
        pkt_line::write_line(stdin, &line)?;
    }
    if send_done {
        trace_packet_git('>', "done");
        pkt_line::write_line(stdin, "done")?;
    }
    pkt_line::write_flush(stdin)?;
    trace_packet_git('>', "0000");
    stdin.flush()?;
    Ok(())
}

pub(crate) fn skip_v2_section_until_boundary(stdout: &mut impl Read) -> Result<()> {
    loop {
        match pkt_line::read_packet(stdout)? {
            None => return Ok(()),
            Some(pkt_line::Packet::Flush) | Some(pkt_line::Packet::Delim) => return Ok(()),
            Some(pkt_line::Packet::Data(line)) => {
                trace_packet_git('<', &line);
            }
            Some(other) => bail!("unexpected v2 section packet: {other:?}"),
        }
    }
}

fn read_sideband_discard_pack(stdout: &mut impl Read) -> Result<()> {
    let mut seen_pack = false;
    loop {
        let Some(payload) = read_pkt_payload_raw(stdout)? else {
            break;
        };
        if payload.is_empty() {
            if seen_pack {
                break;
            }
            continue;
        }
        match payload[0] {
            1 => {
                seen_pack = true;
            }
            2 | 3 => {}
            _ => {
                if !seen_pack && payload.starts_with(b"PACK") {
                    seen_pack = true;
                }
            }
        }
    }
    Ok(())
}

fn read_pkt_payload_raw(r: &mut impl Read) -> std::io::Result<Option<Vec<u8>>> {
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
        0 | 1 | 2 => Ok(None),
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

fn drain_v2_fetch_response(stdout: &mut impl Read, sideband_all: bool) -> Result<()> {
    loop {
        let hdr = match pkt_line::read_packet(stdout)? {
            Some(pkt_line::Packet::Data(s)) => s,
            Some(pkt_line::Packet::Delim) => continue,
            Some(pkt_line::Packet::Flush) => return Ok(()),
            None => return Ok(()),
            Some(other) => bail!("unexpected fetch response: {other:?}"),
        };
        trace_packet_git('<', &hdr);
        match hdr.as_str() {
            "acknowledgments" | "shallow-info" | "wanted-refs" | "packfile-uris" => {
                skip_v2_section_until_boundary(stdout)?;
            }
            "packfile" => {
                if sideband_all {
                    read_sideband_discard_pack(stdout)?;
                } else {
                    let mut junk = Vec::new();
                    stdout.take(64 * 1024 * 1024).read_to_end(&mut junk).ok();
                }
                let _ = pkt_line::read_packet(stdout)?;
                return Ok(());
            }
            other => bail!("unexpected v2 fetch section: {other}"),
        }
    }
}

/// Run `ls-remote` over protocol v2 for a `file://` repository (upload-pack subprocess).
pub(crate) fn ls_remote_file_v2(
    repo_path: &Path,
    upload_pack_cmd: Option<&str>,
    args: &crate::commands::ls_remote::Args,
    server_options: &[String],
) -> Result<()> {
    let default_hash = std::env::var("GIT_DEFAULT_HASH").unwrap_or_else(|_| "sha1".to_owned());
    let mut child = spawn_upload_pack_readonly(upload_pack_cmd, repo_path)?;
    let mut stdin = child.stdin.take().context("upload-pack stdin")?;
    let mut stdout = child.stdout.take().context("upload-pack stdout")?;

    let caps = read_v2_capability_block(&mut stdout)?;
    let bundle_advertised = server_advertises_bundle_uri(&caps);

    if bundle_advertised && transfer_bundle_uri_enabled() {
        let cap_send = cap_lines_for_bundle_request(&caps);
        write_bundle_uri_command(&mut stdin, &cap_send)?;
        drain_bundle_uri_response(&mut stdout)?;
    }

    write_ls_refs_request_for_ls_remote(&mut stdin, &default_hash, args, server_options)?;
    let mut buf = Vec::new();
    read_pkt_lines_until_flush(&mut stdout, &mut buf, 512 * 1024)
        .context("read v2 ls-refs response")?;
    // Close stdin so upload-pack exits instead of blocking for another v2 command.
    drop(stdin);
    let mut drain = Vec::new();
    let _ = stdout.take(64 * 1024).read_to_end(&mut drain);

    let status = child.wait()?;
    if !status.success() {
        bail!(
            "upload-pack exited with status {}",
            status.code().unwrap_or(-1)
        );
    }

    let entries = crate::commands::ls_remote::parse_v2_ls_refs_output(&buf, args)?;
    if entries.is_empty() {
        return Ok(());
    }
    if args.quiet {
        return Ok(());
    }
    for entry in &entries {
        if let Some(target) = &entry.symref_target {
            println!("ref: {target}\t{}", entry.name);
        }
        println!("{}\t{}", entry.oid, entry.name);
    }
    Ok(())
}

/// Optional v2 handshake + bundle-uri + fetch for `file://` clone tests (discards pack).
///
/// `reference_object_dirs` lists the `objects` directories of any `--reference` repos. Wants that
/// are already present in one of those alternates are dropped from the fetch request, matching
/// Git's `everything_local()` — so a `--reference` clone of a ref the reference already has sends
/// no `want` at all (`t5604` "fetched no objects").
pub(crate) fn clone_preflight_file_v2_if_needed(
    source_git_dir: &Path,
    upload_pack_cmd: Option<&str>,
    request_bundle_uri: bool,
    bundle_uri_cli_override: bool,
    server_options: &[String],
    reference_object_dirs: &[PathBuf],
) -> Result<(Option<String>, Option<String>)> {
    if !client_wants_protocol_v2() {
        return Ok((None, None));
    }

    let default_hash = std::env::var("GIT_DEFAULT_HASH").unwrap_or_else(|_| "sha1".to_owned());
    let mut child = spawn_upload_pack_readonly(upload_pack_cmd, source_git_dir)?;
    let mut stdin = child.stdin.take().context("upload-pack stdin")?;
    let mut stdout = child.stdout.take().context("upload-pack stdout")?;

    let caps = read_v2_capability_block(&mut stdout)?;
    let bundle_advertised = server_advertises_bundle_uri(&caps);

    let want_bundle_cmd = bundle_advertised
        && transfer_bundle_uri_enabled()
        && request_bundle_uri
        && !bundle_uri_cli_override;

    if want_bundle_cmd {
        let cap_send = cap_lines_for_bundle_request(&caps);
        write_bundle_uri_command(&mut stdin, &cap_send)?;
        drain_bundle_uri_response(&mut stdout)?;
    }

    write_ls_refs_for_clone(&mut stdin, &default_hash)?;
    let mut ls_buf = Vec::new();
    read_pkt_lines_until_flush(&mut stdout, &mut ls_buf, 512 * 1024)
        .context("read ls-refs for clone preflight")?;
    let (mut wants, mut head_symref, head_oid) = collect_clone_ls_refs_metadata(&ls_buf)?;
    if head_symref.is_none() && should_use_source_head_symref_fallback(source_git_dir) {
        // `serve-v2 ls-refs` can omit unborn HEAD metadata. For file:// clone parity, preserve
        // source HEAD's symbolic target unless the repository explicitly disables unborn ads.
        head_symref = source_head_symref_from_repo_head_file(source_git_dir);
    }
    // Drop wants already available via a `--reference` alternate (Git's `everything_local`): the
    // server need not (and must not, per `t5604`) be asked for objects we can already borrow.
    if !reference_object_dirs.is_empty() && !wants.is_empty() {
        let reference_repos: Vec<Repository> = reference_object_dirs
            .iter()
            .filter_map(|obj_dir| {
                obj_dir
                    .parent()
                    .and_then(|git_dir| Repository::open(git_dir, None).ok())
            })
            .collect();
        if !reference_repos.is_empty() {
            wants.retain(|oid| {
                !reference_repos
                    .iter()
                    .any(|repo| repo.odb.read(oid).is_ok())
            });
        }
    }
    if wants.is_empty() {
        // Close stdin so upload-pack exits; otherwise it stays in serve-loop waiting for the
        // next v2 command and `wait()` can block indefinitely on empty repositories.
        drop(stdin);
        let status = child.wait()?;
        if !status.success() {
            bail!(
                "upload-pack exited with status {}",
                status.code().unwrap_or(-1)
            );
        }
        return Ok((head_symref, head_oid));
    }

    let fetch_supports_sideband_all = v2_fetch_supports_sideband_all(&caps);
    write_v2_fetch_request(
        &mut stdin,
        &default_hash,
        &wants,
        &[],
        fetch_supports_sideband_all,
        true,
        false,
        None,
        server_options,
        None,
        &[],
        None,
        None,
        &[],
        false,
        None,
        &[],
        true,
    )?;
    drop(stdin);
    drain_v2_fetch_response(&mut stdout, fetch_supports_sideband_all)?;

    let status = child.wait()?;
    if !status.success() {
        bail!(
            "upload-pack exited with status {}",
            status.code().unwrap_or(-1)
        );
    }
    Ok((head_symref, head_oid))
}

/// Fetch `bundle.*` lines from a `file://` remote via upload-pack v2.
pub(crate) fn fetch_bundle_uri_lines_file(repo_url: &str) -> Result<Vec<(String, String)>> {
    let path = file_url_to_path(repo_url)?;
    let repo = Repository::open(&path, None)
        .or_else(|_| {
            let gd = path.join(".git");
            Repository::open(&gd, Some(&path))
        })
        .with_context(|| format!("open repository for bundle-uri: {}", path.display()))?;

    let mut child = spawn_upload_pack_readonly(None, &repo.git_dir)?;
    let mut stdin = child.stdin.take().context("upload-pack stdin")?;
    let mut stdout = child.stdout.take().context("upload-pack stdout")?;

    let caps = read_v2_capability_block(&mut stdout)?;
    if !server_advertises_bundle_uri(&caps) {
        bail!("server does not advertise bundle-uri");
    }
    let cap_send = cap_lines_for_bundle_request(&caps);
    write_bundle_uri_command(&mut stdin, &cap_send)?;
    drop(stdin);
    let mut pairs = Vec::new();
    loop {
        match pkt_line::read_packet(&mut stdout).context("read bundle-uri response")? {
            None => break,
            Some(pkt_line::Packet::Flush) => break,
            Some(pkt_line::Packet::Data(line)) => {
                trace_packet_git('<', &line);
                let (k, v) = line
                    .split_once('=')
                    .filter(|(k, v)| !k.is_empty() && !v.is_empty())
                    .ok_or_else(|| anyhow::anyhow!("malformed bundle-uri line: {line}"))?;
                pairs.push((k.to_string(), v.to_string()));
            }
            Some(other) => bail!("unexpected bundle-uri packet: {other:?}"),
        }
    }
    let status = child.wait()?;
    if !status.success() {
        bail!(
            "upload-pack exited with status {}",
            status.code().unwrap_or(-1)
        );
    }
    Ok(pairs)
}

/// Fetch `bundle.*` lines from a `git://` remote via upload-pack v2.
pub(crate) fn fetch_bundle_uri_lines_git(repo_url: &str) -> Result<Vec<(String, String)>> {
    let (mut stdin, mut stdout, _) =
        crate::git_daemon_url::connect_git_daemon_upload_pack(repo_url)?;
    let caps = read_v2_capability_block(&mut stdout)?;
    if !server_advertises_bundle_uri(&caps) {
        bail!("server does not advertise bundle-uri");
    }
    let cap_send = cap_lines_for_bundle_request(&caps);
    write_bundle_uri_command(&mut stdin, &cap_send)?;
    let _ = stdin.shutdown(Shutdown::Write);
    let mut pairs = Vec::new();
    loop {
        match pkt_line::read_packet(&mut stdout).context("read bundle-uri response")? {
            None => break,
            Some(pkt_line::Packet::Flush) => break,
            Some(pkt_line::Packet::Data(line)) => {
                trace_packet_git('<', &line);
                let (k, v) = line
                    .split_once('=')
                    .filter(|(k, v)| !k.is_empty() && !v.is_empty())
                    .ok_or_else(|| anyhow::anyhow!("malformed bundle-uri line: {line}"))?;
                pairs.push((k.to_string(), v.to_string()));
            }
            Some(other) => bail!("unexpected bundle-uri packet: {other:?}"),
        }
    }
    Ok(pairs)
}

fn file_url_to_path(url: &str) -> Result<PathBuf> {
    let s = url.trim();
    let rest = s
        .strip_prefix("file://")
        .ok_or_else(|| anyhow::anyhow!("not a file:// URL: {url}"))?;
    Ok(PathBuf::from(rest))
}
