//! `grit receive-pack` — receive pushed objects (server side).
//!
//! Invoked on the remote side of a push. Advertises refs in pkt-line format (with
//! capabilities), reads ref updates and an optional pack stream from stdin, then
//! updates refs when connectivity checks pass.

use anyhow::{bail, Context, Result};
use clap::Args as ClapArgs;
use grit_lib::check_ref_format::receive_pack_refname_ok;
use grit_lib::config::{parse_bool, ConfigFile, ConfigScope, ConfigSet};
use grit_lib::connectivity::{diagnose_push_connectivity_failure, push_tip_connected_to_refs};
use grit_lib::hide_refs;
use grit_lib::hooks::{run_hook_in_git_dir, HookResult};
use grit_lib::merge_base::is_ancestor;
use grit_lib::objects::ObjectId;
use grit_lib::pack::read_alternates_recursive;
use grit_lib::ref_namespace;
use grit_lib::refs;
use grit_lib::repo::Repository;
use grit_lib::state::{resolve_head, HeadState};
use grit_lib::unpack_objects::pack_bytes_to_object_map;
use std::collections::HashSet;
use std::io::{self, Cursor, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::grit_exe;
use crate::trace2_transfer;
use crate::wire_trace;
use grit_lib::pkt_line::{
    read_packet, write_flush, write_packet_raw, write_sideband_packet, Packet,
};

/// Arguments for `grit receive-pack`.
#[derive(Debug, ClapArgs)]
#[command(about = "Receive pushed objects (server side)")]
pub struct Args {
    /// Path to the repository (bare or non-bare).
    #[arg(value_name = "DIRECTORY")]
    pub directory: PathBuf,

    /// Skip connectivity verification after unpacking (matches `git receive-pack`).
    #[arg(long = "skip-connectivity-check", hide = true)]
    pub skip_connectivity_check: bool,

    /// Test-only: refuse a thin incoming pack (forwarded to index-pack by upstream). Used by
    /// `t5516` 'push --no-thin must produce non-thin pack' to verify the sender honored `--no-thin`.
    #[arg(long = "reject-thin-pack-for-testing", hide = true)]
    pub reject_thin_pack_for_testing: bool,
}

pub fn run(args: Args) -> Result<()> {
    let repo = open_repo(&args.directory).with_context(|| {
        format!(
            "could not open repository at '{}'",
            args.directory.display()
        )
    })?;

    trace2_transfer::emit_negotiated_version_from_git_protocol_env();

    // Use only this repository's `config` so global `core.alternateRefs*` from the
    // environment does not leak across harness tests (matches receive-pack reading repo config).
    let mut config = ConfigSet::new();
    if let Ok(Some(f)) = ConfigFile::from_path(&repo.git_dir.join("config"), ConfigScope::Local) {
        config.merge(&f);
    }
    // Honor `git -c key=value receive-pack` overrides (passed via GIT_CONFIG_PARAMETERS), so the
    // remote side of `send-pack --receive-pack="git -c receive.denyDeletes=false receive-pack"`
    // sees the override on top of the repository's own config — matching `git receive-pack`.
    if let Ok(params) = std::env::var("GIT_CONFIG_PARAMETERS") {
        if !params.trim().is_empty() {
            if let Ok(cmd_file) =
                ConfigFile::from_git_config_parameters(Path::new(":GIT_CONFIG_PARAMETERS"), &params)
            {
                config.merge(&cmd_file);
            }
        }
    }
    let extra_have = collect_alternate_have_oids(&repo, &config)?;

    advertise_refs_phase(&repo, &extra_have)?;

    let hide_patterns = hide_refs::hide_ref_patterns_receive(&config);

    let mut stdin = io::stdin();
    let mut payload = Vec::new();
    stdin.read_to_end(&mut payload)?;

    let mut cursor = Cursor::new(&payload[..]);
    let mut updates: Vec<(String, String, String)> = Vec::new();
    let mut caps_seen = false;
    let mut client_sid_from_caps: Option<String> = None;
    let mut use_sideband = false;

    loop {
        match read_packet(&mut cursor)? {
            None => break,
            Some(Packet::Flush) => break,
            Some(Packet::Delim) | Some(Packet::ResponseEnd) => break,
            Some(Packet::Data(line)) => {
                if !caps_seen {
                    if let Some((_, feats)) = line.split_once('\0') {
                        let feats = feats.trim();
                        if feats.split([' ', '\n']).any(|f| f == "side-band-64k") {
                            use_sideband = true;
                        }
                        if let Some(sid) = trace2_transfer::extract_session_id_feature(feats) {
                            client_sid_from_caps = Some(sid.to_owned());
                        }
                    }
                }
                if let Some((old_h, new_h, refname)) = parse_update_line(&line, !caps_seen) {
                    caps_seen = true;
                    updates.push((old_h, new_h, refname));
                }
            }
        }
    }

    // `git receive-pack` routes hook output and diagnostics to sideband band 2 when the client
    // negotiated `side-band-64k`, and wraps the report-status stream in band 1. The client
    // demultiplexes band 2/3 to its stderr (prefixed `remote: `) and band 1 to its status parser.
    let mut diag = DiagSink::new(use_sideband);

    if let Some(ref sid) = client_sid_from_caps {
        trace2_transfer::emit_client_sid(sid);
    }

    let zero_oid_early = "0".repeat(40);
    let mut hidden_rejects: Vec<String> = Vec::new();
    for (_old_h, new_h, refname) in &updates {
        let full = ref_namespace::storage_ref_name(refname);
        if hide_refs::ref_is_hidden(refname, &full, &hide_patterns) {
            if new_h == &zero_oid_early {
                diag.line("error: deny deleting a hidden ref");
            } else {
                diag.line("error: deny updating a hidden ref");
            }
            hidden_rejects.push(refname.clone());
        }
    }

    let pack_start = cursor.position() as usize;
    let tail = &payload[pack_start..];
    // After the command flush, git send-pack writes the raw packfile bytes (starts with "PACK").
    // Do not feed those through the pkt-line demuxer — it would mis-parse the length prefix.
    let (pack_data, sideband_stderr) = if tail.starts_with(b"PACK") {
        (tail.to_vec(), Vec::new())
    } else {
        demux_input_tail(tail)
    };
    if !sideband_stderr.is_empty() {
        let _ = io::stderr().write_all(&sideband_stderr);
    }

    let zero_oid = "0".repeat(40);
    let has_pack = !pack_data.is_empty() && pack_data.len() > 12 && pack_data.starts_with(b"PACK");

    let mut pack_map = None;
    let mut pack_parse_err: Option<String> = None;

    // `--reject-thin-pack-for-testing`: refuse a thin incoming pack so the suite can verify a
    // `git push --no-thin` actually produced a self-contained pack (t5516 'push --no-thin').
    if has_pack
        && args.reject_thin_pack_for_testing
        && grit_lib::unpack_objects::pack_is_thin(&pack_data, repo.odb.hash_algo())
    {
        pack_parse_err = Some("fatal: pack has unresolved deltas (thin pack rejected)".to_owned());
    }
    // Thin packs may not resolve fully in-memory against an empty ODB; skip this when we will
    // not run connectivity anyway (`git receive-pack` still unpacks via unpack-objects/index-pack).
    if has_pack && !args.skip_connectivity_check && pack_parse_err.is_none() {
        match pack_bytes_to_object_map(&pack_data, &repo.odb) {
            Ok(m) => pack_map = Some(m),
            Err(e) => pack_parse_err = Some(format!("{e:#}")),
        }
    }

    let mut connectivity_failed: Vec<String> = Vec::new();
    let mut traverse_err: Option<String> = None;

    if !args.skip_connectivity_check {
        if let Some(ref err) = pack_parse_err {
            for (_old_hex, new_hex, refname) in &updates {
                if new_hex != &zero_oid && !hidden_rejects.iter().any(|r| r == refname) {
                    connectivity_failed.push(refname.clone());
                }
            }
            traverse_err = Some(err.clone());
        } else {
            let pack_ref = pack_map.as_ref();
            for (_old_hex, new_hex, refname) in &updates {
                if new_hex == &zero_oid {
                    continue;
                }
                if hidden_rejects.iter().any(|r| r == refname) {
                    continue;
                }
                let tip = match ObjectId::from_hex(new_hex) {
                    Ok(o) => o,
                    Err(_) => {
                        connectivity_failed.push(refname.clone());
                        continue;
                    }
                };
                match push_tip_connected_to_refs(&repo, tip, &extra_have, pack_ref) {
                    Ok(true) => {}
                    Ok(false) => {
                        connectivity_failed.push(refname.clone());
                        if traverse_err.is_none() {
                            if let Ok(Some((missing, at))) = diagnose_push_connectivity_failure(
                                &repo,
                                tip,
                                &extra_have,
                                pack_ref,
                            ) {
                                traverse_err = Some(format!(
                                    "Could not read {}\nfatal: Failed to traverse parents of commit {}",
                                    missing.to_hex(),
                                    at.to_hex()
                                ));
                            }
                        }
                    }
                    Err(e) => {
                        let msg = format!("{e:#}");
                        connectivity_failed.push(refname.clone());
                        if traverse_err.is_none() {
                            traverse_err = Some(msg);
                        }
                    }
                }
            }
        }
    }

    let should_unpack_to_odb = has_pack
        && pack_parse_err.is_none()
        && (args.skip_connectivity_check || connectivity_failed.is_empty());

    let mut unpack_to_odb_err: Option<String> = None;
    if should_unpack_to_odb {
        let remote_for_ingest = ConfigSet::load(Some(&repo.git_dir), false)?;
        if let Err(e) = crate::receive_ingest::ingest_received_pack(
            &repo.git_dir,
            &pack_data,
            &remote_for_ingest,
            !args.skip_connectivity_check,
        ) {
            unpack_to_odb_err = Some(format!("{e:#}"));
        }
    }

    if let Some(ref e) = unpack_to_odb_err {
        for (_old_hex, new_hex, refname) in &updates {
            if new_hex != &zero_oid && !hidden_rejects.iter().any(|r| r == refname) {
                if !connectivity_failed.iter().any(|r| r == refname) {
                    connectivity_failed.push(refname.clone());
                }
            }
        }
        if traverse_err.is_none() {
            traverse_err = Some(e.clone());
        }
    }

    let unpack_status: Vec<u8> = if !has_pack {
        b"unpack ok\n".to_vec()
    } else if pack_parse_err.is_some() {
        b"unpack unpacker error\n".to_vec()
    } else if !args.skip_connectivity_check && !connectivity_failed.is_empty() {
        b"unpack ok\n".to_vec()
    } else if unpack_to_odb_err.is_some() {
        b"unpack unpacker error\n".to_vec()
    } else {
        b"unpack ok\n".to_vec()
    };

    // Connectivity/unpack failure diagnostics are written to the receive-pack process's real
    // stderr even under side-band (matches `git receive-pack`, where these come from the unpack
    // subprocess and are not relayed over band 2). Only the report itself is band-1 framed.
    if let Some(ref e) = traverse_err {
        let stderr = io::stderr();
        let mut err = stderr.lock();
        for line in e.lines() {
            if line.starts_with("fatal: ") {
                let _ = writeln!(err, "{line}");
            } else {
                let _ = writeln!(err, "error: {line}");
            }
        }
        let _ = err.flush();
    }

    // Refs already rejected by connectivity/hidden checks never reach the hook stage.
    let pre_rejected: Vec<(String, &'static str)> = updates
        .iter()
        .filter_map(|(_o, new_hex, refname)| {
            if new_hex == &zero_oid {
                return None;
            }
            if hidden_rejects.iter().any(|r| r == refname) {
                Some((refname.clone(), "failed to update ref"))
            } else if connectivity_failed.iter().any(|r| r == refname) {
                Some((refname.clone(), "missing necessary objects"))
            } else {
                None
            }
        })
        .collect();

    // When any ref already failed an earlier gate, do not run hooks/update refs: report and exit.
    let ref_outcomes: Vec<RefOutcome> =
        if !connectivity_failed.is_empty() || !hidden_rejects.is_empty() {
            updates
                .iter()
                .map(|(_o, new_hex, refname)| {
                    let is_delete = new_hex == &zero_oid;
                    match pre_rejected.iter().find(|(r, _)| r == refname) {
                        Some((_, reason)) => {
                            RefOutcome::rejected(refname, reason).with_delete(is_delete)
                        }
                        None => RefOutcome::accepted(refname).with_delete(is_delete),
                    }
                })
                .collect()
        } else {
            run_hooks_and_update_refs(&repo, &config, &updates, &zero_oid, &mut diag)?
        };

    write_status_lines(&ref_outcomes, &zero_oid, &unpack_status, &mut diag)?;
    diag.finish()?;

    Ok(())
}

/// Outcome of a single ref update after all gates and hooks have run.
struct RefOutcome {
    refname: String,
    /// `None` if the ref was accepted; otherwise the report-status `ng` reason string.
    reject_reason: Option<String>,
    /// Whether this update is a deletion (`new` is the null OID).
    is_delete: bool,
}

impl RefOutcome {
    fn accepted(refname: &str) -> Self {
        Self {
            refname: refname.to_owned(),
            reject_reason: None,
            is_delete: false,
        }
    }

    fn rejected(refname: &str, reason: &str) -> Self {
        Self {
            refname: refname.to_owned(),
            reject_reason: Some(reason.to_owned()),
            is_delete: false,
        }
    }

    fn with_delete(mut self, is_delete: bool) -> Self {
        self.is_delete = is_delete;
        self
    }
}

/// Sink for receive-pack diagnostic/hook output and the report-status stream.
///
/// With `side-band-64k` negotiated, diagnostics go to band 2 and the report to band 1 on stdout
/// (the client demultiplexes, prefixing band 2/3 with `remote: `). Otherwise diagnostics go to
/// this process's stderr and the report is written as bare pkt-lines on stdout.
struct DiagSink {
    use_sideband: bool,
}

impl DiagSink {
    fn new(use_sideband: bool) -> Self {
        Self { use_sideband }
    }

    /// Emit one diagnostic line (no trailing newline expected in `line`).
    fn line(&mut self, line: &str) {
        let mut buf = line.as_bytes().to_vec();
        buf.push(b'\n');
        self.bytes(&buf);
    }

    /// Emit raw diagnostic bytes (e.g. captured hook stdout/stderr) verbatim.
    fn bytes(&mut self, data: &[u8]) {
        if data.is_empty() {
            return;
        }
        if self.use_sideband {
            let stdout = io::stdout();
            let mut out = stdout.lock();
            for chunk in data.chunks(65515) {
                let _ = write_sideband_packet(&mut out, 2, chunk);
            }
            let _ = out.flush();
        } else {
            let _ = io::stderr().write_all(data);
        }
    }

    /// Write the report-status pkt-line stream (already framed) to the wire.
    fn write_report(&mut self, report: &[u8]) -> Result<()> {
        let stdout = io::stdout();
        let mut out = stdout.lock();
        if self.use_sideband {
            for chunk in report.chunks(65515) {
                write_sideband_packet(&mut out, 1, chunk)?;
            }
        } else {
            out.write_all(report)?;
        }
        out.flush()?;
        Ok(())
    }

    /// Final flush packet that closes the sideband stream (no-op without sideband).
    fn finish(&mut self) -> Result<()> {
        if self.use_sideband {
            let stdout = io::stdout();
            let mut out = stdout.lock();
            write_flush(&mut out)?;
            out.flush()?;
        }
        Ok(())
    }
}

fn write_status_lines(
    outcomes: &[RefOutcome],
    _zero_oid: &str,
    unpack_status: &[u8],
    diag: &mut DiagSink,
) -> Result<()> {
    let mut report: Vec<u8> = Vec::new();
    write_packet_raw(&mut report, unpack_status)?;
    // `git receive-pack` reports the status of every ref command, including deletions
    // (see `report()` in builtin/receive-pack.c): `ok <ref>` for accepted updates and
    // `ng <ref> <reason>` for rejected ones.
    for outcome in outcomes {
        let refname = &outcome.refname;
        match &outcome.reject_reason {
            Some(reason) => {
                write_packet_raw(&mut report, format!("ng {refname} {reason}\n").as_bytes())?;
            }
            None => {
                write_packet_raw(&mut report, format!("ok {refname}\n").as_bytes())?;
            }
        }
    }
    write_flush(&mut report)?;
    diag.write_report(&report)?;
    Ok(())
}

fn parse_update_line(line: &str, first: bool) -> Option<(String, String, String)> {
    let line = line.trim_end_matches('\n');
    let content = if first {
        line.split('\0').next()?.trim()
    } else {
        line.trim()
    };
    let parts: Vec<&str> = content.splitn(3, ' ').collect();
    if parts.len() != 3 {
        return None;
    }
    Some((
        parts[0].to_owned(),
        parts[1].to_owned(),
        parts[2].to_owned(),
    ))
}

fn demux_input_tail(data: &[u8]) -> (Vec<u8>, Vec<u8>) {
    if data.starts_with(b"PACK") {
        return (data.to_vec(), Vec::new());
    }
    let mut pack = Vec::new();
    let mut stderr_buf = Vec::new();
    let mut i = 0usize;
    while i + 4 <= data.len() {
        let len_str = match std::str::from_utf8(&data[i..i + 4]) {
            Ok(s) => s,
            Err(_) => break,
        };
        let Ok(pkt_len) = usize::from_str_radix(len_str, 16) else {
            break;
        };
        if pkt_len == 0 {
            i += 4;
            continue;
        }
        if pkt_len < 4 || i + pkt_len > data.len() {
            break;
        }
        let payload_len = pkt_len - 4;
        let payload = &data[i + 4..i + pkt_len];
        i += pkt_len;
        if payload_len == 0 || payload.is_empty() {
            continue;
        }
        match payload[0] {
            1 => pack.extend_from_slice(&payload[1..]),
            2 => stderr_buf.extend_from_slice(&payload[1..]),
            _ => {}
        }
    }
    if pack.is_empty() && !data.is_empty() {
        (data.to_vec(), stderr_buf)
    } else {
        (pack, stderr_buf)
    }
}

fn collect_alternate_have_oids(repo: &Repository, config: &ConfigSet) -> Result<HashSet<ObjectId>> {
    let mut out = HashSet::new();
    let objects_dir = repo.git_dir.join("objects");
    let alternates = read_alternates_recursive(&objects_dir).unwrap_or_default();
    let recv_git_dir = repo.git_dir.as_path();
    for alt_objects in alternates {
        let Some(alt_git_dir) = alt_objects.parent().map(PathBuf::from) else {
            continue;
        };
        if !alt_git_dir.join("refs").is_dir() {
            continue;
        }
        let alt = alt_git_dir.as_path();
        // Prefer explicit prefixes when both are set: the harness may leave a stale
        // `core.alternateRefsCommand` in the repo between cases while adding prefixes.
        if let Some(prefixes) = config.get("core.alternateRefsPrefixes") {
            for line in run_for_each_ref_lines(recv_git_dir, alt, Some(&prefixes))? {
                if let Ok(oid) = ObjectId::from_hex(line.trim()) {
                    out.insert(oid);
                }
            }
        } else if let Some(cmdline) = config.get("core.alternateRefsCommand") {
            for line in run_alternate_command(recv_git_dir, alt, &cmdline)? {
                if let Ok(oid) = ObjectId::from_hex(line.trim()) {
                    out.insert(oid);
                }
            }
        } else {
            for (_, oid) in refs::list_refs(alt, "refs/")? {
                out.insert(oid);
            }
        }
    }
    Ok(out)
}

fn run_alternate_command(
    receiving_git_dir: &Path,
    alternate_git_dir: &Path,
    command: &str,
) -> Result<Vec<String>> {
    // Match git's `fill_alternate_refs_command`: `use_shell` with the configured command
    // as the shell script and the alternate repository path as `$1` (see git/odb.c).
    let script = format!("{} \"$1\"", command.trim_end());
    let mut c = Command::new("sh");
    c.current_dir(receiving_git_dir)
        .arg("-c")
        .arg(&script)
        .arg("sh")
        .arg(alternate_git_dir.as_os_str())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let out = c.output().context("running core.alternateRefsCommand")?;
    if !out.status.success() {
        return Ok(Vec::new());
    }
    Ok(out
        .stdout
        .split(|b| *b == b'\n')
        .filter_map(|l| std::str::from_utf8(l).ok().map(|s| s.to_owned()))
        .collect())
}

fn run_for_each_ref_lines(
    exec_cwd: &Path,
    git_dir_env: &Path,
    prefixes: Option<&str>,
) -> Result<Vec<String>> {
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("grit"));
    let mut c = Command::new(exe);
    c.current_dir(exec_cwd)
        .arg(format!("--git-dir={}", git_dir_env.display()))
        .args(["for-each-ref", "--format=%(objectname)"]);
    if let Some(p) = prefixes {
        c.arg("--");
        for part in p.split_whitespace() {
            c.arg(part);
        }
    }
    c.stdout(Stdio::piped()).stderr(Stdio::null());
    let out = c
        .output()
        .context("running for-each-ref for alternate refs")?;
    if !out.status.success() {
        return Ok(Vec::new());
    }
    Ok(out
        .stdout
        .split(|b| *b == b'\n')
        .filter_map(|l| std::str::from_utf8(l).ok().map(|s| s.to_owned()))
        .collect())
}

fn advertise_refs_phase(repo: &Repository, extra_have: &HashSet<ObjectId>) -> Result<()> {
    let stdout = io::stdout();
    let mut out = stdout.lock();
    let version = crate::version_string();
    let cfg = ConfigSet::load(Some(&repo.git_dir), false).unwrap_or_default();
    let object_format = cfg
        .get("extensions.objectformat")
        .or_else(|| cfg.get("extensions.objectFormat"))
        .map(|s| s.to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "sha1".to_owned());
    let mut caps = format!(
        "report-status report-status-v2 delete-refs side-band-64k quiet ofs-delta \
         object-format={object_format} agent=grit/{version}"
    );
    if trace2_transfer::transfer_advertise_sid_enabled(&repo.git_dir) {
        let sid = trace2_transfer::trace2_session_id_wire_once();
        caps.push_str(" session-id=");
        caps.push_str(&sid);
    }
    let hide = hide_refs::hide_ref_patterns_receive(&cfg);

    // `git receive-pack` does not advertise `HEAD`; it iterates only the ref store
    // (`refs_for_each_ref_ext` in `write_head_info`). Each local ref is advertised under its
    // logical name regardless of duplicate object ids; only out-of-namespace refs and alternate
    // refs are folded into de-duplicated `.have` lines (matches `show_ref_cb`).
    let mut first = true;
    let namespace_prefix = ref_namespace::ref_storage_prefix();
    let mut seen_have: HashSet<ObjectId> = HashSet::new();
    let all_refs = refs::list_refs_physical(&repo.git_dir, "refs/")?;
    for (refname, oid) in &all_refs {
        // Determine the advertised name following Git's `strip_namespace`:
        //  - no active namespace  -> the ref's own name (always advertised)
        //  - in the active namespace -> its logical name (always advertised)
        //  - outside the active namespace -> `.have` (de-duplicated by object id)
        let display = match &namespace_prefix {
            None => {
                let full = ref_namespace::storage_ref_name(refname);
                if hide_refs::ref_is_hidden(refname, &full, &hide) {
                    continue;
                }
                let _ = seen_have.insert(*oid);
                refname.clone()
            }
            Some(_) => {
                if let Some(logical) = ref_namespace::logical_ref_name_from_storage(refname) {
                    let full = ref_namespace::storage_ref_name(&logical);
                    if hide_refs::ref_is_hidden(&logical, &full, &hide) {
                        continue;
                    }
                    let _ = seen_have.insert(*oid);
                    logical
                } else {
                    // Out of the active namespace: advertise as a de-duplicated `.have`.
                    if !seen_have.insert(*oid) {
                        continue;
                    }
                    ".have".to_owned()
                }
            }
        };
        if first {
            let line = format!("{} {display}\0{caps}\n", oid.to_hex());
            wire_trace::trace_packet_receive_pack('>', line.trim_end_matches('\n'));
            let len = 4 + line.len();
            write!(out, "{:04x}{}", len, line)?;
            first = false;
        } else {
            let line = format!("{} {display}\n", oid.to_hex());
            wire_trace::trace_packet_receive_pack('>', line.trim_end_matches('\n'));
            let len = 4 + line.len();
            write!(out, "{:04x}{}", len, line)?;
        }
    }

    for h in extra_have {
        if seen_have.insert(*h) {
            let line = format!("{} .have\n", h.to_hex());
            wire_trace::trace_packet_receive_pack('>', line.trim_end_matches('\n'));
            let len = 4 + line.len();
            write!(out, "{:04x}{}", len, line)?;
        }
    }

    if first {
        let zero_width = if object_format == "sha256" { 64 } else { 40 };
        let zero = "0".repeat(zero_width);
        let line = format!("{zero} capabilities^{{}}\0{caps}\n");
        wire_trace::trace_packet_receive_pack('>', line.trim_end_matches('\n'));
        let len = 4 + line.len();
        write!(out, "{:04x}{}", len, line)?;
    }

    write_flush(&mut out)?;
    out.flush()?;
    Ok(())
}

fn open_repo(path: &Path) -> Result<Repository> {
    if let Ok(repo) = Repository::open(path, None) {
        return Ok(repo);
    }
    let git_dir = path.join(".git");
    Repository::open(&git_dir, Some(path)).map_err(Into::into)
}

fn reference_transaction_stdin(updates: &[&(String, String, String)], zero_oid: &str) -> String {
    let lines: Vec<String> = updates
        .iter()
        .map(|(old_hex, new_hex, refname)| {
            let old_display = if old_hex == zero_oid {
                zero_oid.to_owned()
            } else {
                (*old_hex).clone()
            };
            format!("{old_display} {new_hex} {refname}")
        })
        .collect();
    if lines.is_empty() {
        String::new()
    } else {
        format!("{}\n", lines.join("\n"))
    }
}

fn run_hooks_and_update_refs(
    repo: &Repository,
    remote_config: &ConfigSet,
    updates: &[(String, String, String)],
    zero_oid: &str,
    diag: &mut DiagSink,
) -> Result<Vec<RefOutcome>> {
    let hook_stdin = updates
        .iter()
        .map(|(old_hex, new_hex, refname)| format!("{old_hex} {new_hex} {refname}\n"))
        .collect::<String>();

    let mut push_option_env_owned: Vec<(String, String)> = Vec::new();
    if let Ok(count_raw) = std::env::var("GIT_PUSH_OPTION_COUNT") {
        if let Ok(count) = count_raw.parse::<usize>() {
            push_option_env_owned.push(("GIT_PUSH_OPTION_COUNT".to_owned(), count.to_string()));
            for idx in 0..count {
                let key = format!("GIT_PUSH_OPTION_{idx}");
                if let Ok(val) = std::env::var(&key) {
                    push_option_env_owned.push((key, val));
                }
            }
        }
    }
    let push_option_env: Vec<(&str, &str)> = push_option_env_owned
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();

    let deny_deletes = config_bool_any(
        remote_config,
        &["receive.denyDeletes", "receive.denydeletes"],
        false,
    );
    let deny_nff = config_bool_any(
        remote_config,
        &["receive.denyNonFastForwards", "receive.denynonfastforwards"],
        false,
    );
    let head_ref_for_delete = if repo.is_bare() {
        None
    } else {
        match resolve_head(&repo.git_dir) {
            Ok(HeadState::Branch { refname, .. }) => Some(refname),
            _ => None,
        }
    };

    let (pre_receive_result, pre_receive_output) = run_hook_in_git_dir(
        repo,
        "pre-receive",
        &[],
        Some(hook_stdin.as_bytes()),
        &push_option_env,
    );
    diag.bytes(&pre_receive_output);
    if let HookResult::Failed(_code) = pre_receive_result {
        bail!("pre-receive hook declined the push");
    }

    // Funny ref names are rejected in `update()` before any ref-transaction work in Git.
    // Classify them here so reference-transaction and update hooks never see them.
    let mut funny_refnames: HashSet<String> = HashSet::new();
    for (_old_hex, new_hex, refname) in updates {
        let is_delete = new_hex == zero_oid;
        if !receive_pack_refname_ok(refname, is_delete) {
            funny_refnames.insert(refname.clone());
        }
    }

    let hookable_updates: Vec<&(String, String, String)> = updates
        .iter()
        .filter(|(_, _, refname)| !funny_refnames.contains(refname))
        .collect();

    let ref_tx_stdin = reference_transaction_stdin(&hookable_updates, zero_oid);

    if !hookable_updates.is_empty() {
        let (tx_preparing_result, tx_preparing_output) = run_hook_in_git_dir(
            repo,
            "reference-transaction",
            &["preparing"],
            Some(ref_tx_stdin.as_bytes()),
            &push_option_env,
        );
        diag.bytes(&tx_preparing_output);
        if let HookResult::Failed(_code) = tx_preparing_result {
            bail!("reference-transaction hook declined the update");
        }

        let (tx_prepared_result, tx_prepared_output) = run_hook_in_git_dir(
            repo,
            "reference-transaction",
            &["prepared"],
            Some(ref_tx_stdin.as_bytes()),
            &push_option_env,
        );
        diag.bytes(&tx_prepared_output);
        if let HookResult::Failed(_code) = tx_prepared_result {
            let _ = run_hook_in_git_dir(
                repo,
                "reference-transaction",
                &["aborted"],
                Some(ref_tx_stdin.as_bytes()),
                &push_option_env,
            );
            bail!("reference-transaction hook declined the update");
        }
    }

    // Per-ref acceptance: `git receive-pack` rejects individual commands (policy or update-hook
    // declines) and continues with the rest, rather than aborting the whole push. Accepted refs
    // are committed; rejected refs are reported via `ng <ref> <reason>` in the status report.
    let mut outcomes: Vec<RefOutcome> = Vec::with_capacity(updates.len());
    let mut accepted: Vec<(String, String, String)> = Vec::new();
    for (old_hex, new_hex, refname) in updates {
        let is_delete = new_hex == zero_oid;

        if funny_refnames.contains(refname.as_str()) {
            diag.line(&format!(
                "error: refusing to update funny ref '{refname}' remotely"
            ));
            outcomes.push(RefOutcome::rejected(refname, "funny refname").with_delete(is_delete));
            continue;
        }

        match check_receive_update_policy(
            repo,
            remote_config,
            refname,
            old_hex,
            new_hex,
            deny_deletes,
            deny_nff,
            head_ref_for_delete.as_deref(),
            diag,
        )? {
            Some(reason) => {
                outcomes.push(RefOutcome::rejected(refname, reason).with_delete(is_delete));
                continue;
            }
            None => {}
        }

        let old_for_update = refs::resolve_ref(&repo.git_dir, refname)
            .map(|oid| oid.to_hex())
            .unwrap_or_else(|_| zero_oid.to_owned());
        let (update_result, update_output) = run_hook_in_git_dir(
            repo,
            "update",
            &[refname, &old_for_update, new_hex],
            None,
            &push_option_env,
        );
        diag.bytes(&update_output);
        if let HookResult::Failed(_code) = update_result {
            diag.line(&format!("error: hook declined to update {refname}"));
            outcomes.push(RefOutcome::rejected(refname, "hook declined").with_delete(is_delete));
            continue;
        }

        if is_delete {
            refs::delete_ref(&repo.git_dir, refname)
                .with_context(|| format!("deleting ref {refname}"))?;
        } else {
            let new_oid =
                ObjectId::from_hex(new_hex).with_context(|| format!("invalid oid: {new_hex}"))?;
            refs::write_ref(&repo.git_dir, refname, &new_oid)
                .with_context(|| format!("updating ref {refname}"))?;
            if let Some(wt) = repo.work_tree.as_ref() {
                if let Ok(HeadState::Branch {
                    refname: head_br, ..
                }) = resolve_head(&repo.git_dir)
                {
                    if head_br == *refname {
                        let deny = read_receive_deny_current(remote_config);
                        if matches!(deny, ReceiveDenyAction::UpdateInstead) {
                            checkout_worktree_to_commit(wt, new_oid)?;
                        }
                    }
                }
            }
        }

        outcomes.push(RefOutcome::accepted(refname).with_delete(is_delete));
        accepted.push((old_hex.clone(), new_hex.clone(), refname.clone()));
    }

    if !accepted.is_empty() && !hookable_updates.is_empty() {
        let committed_refs: Vec<&(String, String, String)> = accepted.iter().collect();
        let committed_stdin = reference_transaction_stdin(&committed_refs, zero_oid);
        let (tx_committed_result, tx_committed_output) = run_hook_in_git_dir(
            repo,
            "reference-transaction",
            &["committed"],
            Some(committed_stdin.as_bytes()),
            &push_option_env,
        );
        diag.bytes(&tx_committed_output);
        if let HookResult::Failed(_code) = tx_committed_result {
            // committed hook exit status is ignored (matches githooks(5)).
        }
    }

    // post-receive and post-update only see the refs that were actually updated (matches
    // `git receive-pack`). When nothing was committed, neither hook runs.
    if !accepted.is_empty() {
        let post_receive_stdin = accepted
            .iter()
            .map(|(old_hex, new_hex, refname)| format!("{old_hex} {new_hex} {refname}\n"))
            .collect::<String>();
        let (post_receive_result, post_receive_output) = run_hook_in_git_dir(
            repo,
            "post-receive",
            &[],
            Some(post_receive_stdin.as_bytes()),
            &push_option_env,
        );
        diag.bytes(&post_receive_output);
        if let HookResult::Failed(_code) = post_receive_result {
            // post-receive is informational only.
        }

        let post_update_arg_strings: Vec<String> = accepted
            .iter()
            .map(|(_, _, refname)| refname.clone())
            .collect();
        let post_update_args: Vec<&str> =
            post_update_arg_strings.iter().map(|s| s.as_str()).collect();
        let (post_update_result, post_update_output) = run_hook_in_git_dir(
            repo,
            "post-update",
            &post_update_args,
            None,
            &push_option_env,
        );
        diag.bytes(&post_update_output);
        if let HookResult::Failed(_code) = post_update_result {
            // post-update failures are ignored (matches receive-pack).
        }
    }

    let auto_gc = config_bool_any(remote_config, &["receive.autoGc", "receive.autogc"], true);
    if auto_gc && !accepted.is_empty() {
        run_auto_maintenance_quiet(&repo.git_dir);
    }

    Ok(outcomes)
}

fn config_bool_any(cfg: &ConfigSet, keys: &[&str], default: bool) -> bool {
    for k in keys {
        if let Some(v) = cfg.get_bool(k) {
            return v.unwrap_or(default);
        }
    }
    default
}

#[derive(Clone, Copy)]
enum ReceiveDenyAction {
    Unconfigured,
    Ignore,
    Warn,
    Refuse,
    UpdateInstead,
}

fn parse_receive_deny_action(value: Option<&str>) -> ReceiveDenyAction {
    match value.map(str::trim) {
        None => ReceiveDenyAction::Ignore,
        Some(s) if s.eq_ignore_ascii_case("ignore") => ReceiveDenyAction::Ignore,
        Some(s) if s.eq_ignore_ascii_case("warn") => ReceiveDenyAction::Warn,
        Some(s) if s.eq_ignore_ascii_case("refuse") => ReceiveDenyAction::Refuse,
        Some(s) if s.eq_ignore_ascii_case("updateinstead") => ReceiveDenyAction::UpdateInstead,
        Some(s) => match parse_bool(s) {
            Ok(true) => ReceiveDenyAction::Refuse,
            Ok(false) => ReceiveDenyAction::Ignore,
            Err(_) => ReceiveDenyAction::Ignore,
        },
    }
}

fn read_receive_deny_delete_current(cfg: &ConfigSet) -> ReceiveDenyAction {
    let v = cfg
        .get("receive.denyDeleteCurrent")
        .or_else(|| cfg.get("receive.denydeletecurrent"));
    match v.as_deref().map(str::trim) {
        None => ReceiveDenyAction::Unconfigured,
        Some(s) => parse_receive_deny_action(Some(s)),
    }
}

fn read_receive_deny_current(cfg: &ConfigSet) -> ReceiveDenyAction {
    let v = cfg
        .get("receive.denyCurrentBranch")
        .or_else(|| cfg.get("receive.denycurrentbranch"));
    match v.as_deref().map(str::trim) {
        None => ReceiveDenyAction::Unconfigured,
        Some(s) => parse_receive_deny_action(Some(s)),
    }
}

fn checkout_worktree_to_commit(wt: &Path, oid: ObjectId) -> Result<()> {
    let status = Command::new(grit_exe::grit_executable())
        .current_dir(wt)
        .args(["checkout", "-q", "-f", &oid.to_hex()])
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .status()
        .context("spawn grit checkout for receive.denyCurrentBranch updateInstead")?;
    if !status.success() {
        bail!("failed to update work tree to {}", oid.to_hex());
    }
    Ok(())
}

/// Apply the receive-pack acceptance policy for one ref.
///
/// Returns `Ok(None)` when the update is allowed, `Ok(Some(reason))` with the report-status
/// `ng` reason when this single ref is rejected (matching `git receive-pack`'s per-command
/// rejection), or `Err` only for genuine internal errors. Human-readable diagnostics are routed
/// through `diag` (band 2 under side-band, otherwise stderr).
#[allow(clippy::too_many_arguments)]
fn check_receive_update_policy(
    repo: &Repository,
    cfg: &ConfigSet,
    refname: &str,
    old_hex: &str,
    new_hex: &str,
    deny_deletes: bool,
    deny_nff: bool,
    head_branch: Option<&str>,
    diag: &mut DiagSink,
) -> Result<Option<&'static str>> {
    let zero_oid = "0".repeat(40);
    let is_delete = new_hex == zero_oid;
    let had_old = old_hex != zero_oid;

    if is_delete && had_old && deny_deletes && refname.starts_with("refs/heads/") {
        diag.line(&format!("error: denying ref deletion for {refname}"));
        return Ok(Some("deletion prohibited"));
    }

    if is_delete && had_old {
        if let Some(head) = head_branch {
            if refname == head {
                let deny = read_receive_deny_delete_current(cfg);
                match deny {
                    ReceiveDenyAction::Ignore => {}
                    ReceiveDenyAction::Warn => {
                        diag.line("warning: deleting the current branch");
                    }
                    ReceiveDenyAction::Refuse | ReceiveDenyAction::UpdateInstead => {
                        diag.line(&format!(
                            "error: refusing to delete the current branch: {refname}"
                        ));
                        return Ok(Some("deletion of the current branch prohibited"));
                    }
                    ReceiveDenyAction::Unconfigured => {
                        diag.line(
                            "error: By default, deleting the current branch is denied, because the next\n\
                             'git clone' won't result in any file checked out, causing confusion.\n\
                             \n\
                             You can set 'receive.denyDeleteCurrent' configuration variable to\n\
                             'warn' or 'ignore' in the remote repository to allow deleting the\n\
                             current branch, with or without a warning message.\n\
                             \n\
                             To squelch this message, you can set it to 'refuse'.",
                        );
                        diag.line(&format!(
                            "error: refusing to delete the current branch: {refname}"
                        ));
                        return Ok(Some("deletion of the current branch prohibited"));
                    }
                }
            }
        }
    }

    if deny_nff && !is_delete && had_old && refname.starts_with("refs/heads/") {
        let old_oid =
            ObjectId::from_hex(old_hex).with_context(|| format!("invalid old oid on {refname}"))?;
        let new_oid =
            ObjectId::from_hex(new_hex).with_context(|| format!("invalid new oid on {refname}"))?;
        if old_oid != new_oid && !is_ancestor(repo, old_oid, new_oid)? {
            diag.line(&format!(
                "error: denying non-fast-forward {refname} (you should pull first)"
            ));
            return Ok(Some("non-fast-forward"));
        }
    }

    if !is_delete && !repo.is_bare() {
        if let Some(head) = head_branch {
            if refname == head {
                let head_oid_ok = refs::resolve_ref(&repo.git_dir, head).is_ok();
                let deny = read_receive_deny_current(cfg);
                if !head_oid_ok && matches!(deny, ReceiveDenyAction::UpdateInstead) {
                    return Ok(None);
                }
                match deny {
                    ReceiveDenyAction::Ignore | ReceiveDenyAction::UpdateInstead => {}
                    ReceiveDenyAction::Warn => {
                        diag.line("warning: updating the current branch");
                    }
                    ReceiveDenyAction::Refuse => {
                        diag.line(&format!(
                            "error: refusing to update checked out branch: {refname}"
                        ));
                        return Ok(Some("branch is currently checked out"));
                    }
                    ReceiveDenyAction::Unconfigured => {
                        diag.line(&format!(
                            "error: refusing to update checked out branch: {refname}\n\
                             error: By default, updating the current branch in a non-bare repository\n\
                             is denied, because it will make the index and work tree inconsistent\n\
                             with what you pushed, and will require 'git reset --hard' to match\n\
                             the work tree to HEAD.\n\
                             \n\
                             You can set the 'receive.denyCurrentBranch' configuration variable\n\
                             to 'ignore' or 'warn' in the remote repository to allow pushing into\n\
                             its current branch; however, this is not recommended unless you\n\
                             arranged to update its work tree to match what you pushed in some\n\
                             other way.\n\
                             \n\
                             To squelch this message and still keep the default behaviour, set\n\
                             'receive.denyCurrentBranch' configuration variable to 'refuse'."
                        ));
                        return Ok(Some("branch is currently checked out"));
                    }
                }
            }
        }
    }

    Ok(None)
}

fn run_auto_maintenance_quiet(git_dir: &Path) {
    let maintenance_auto = ConfigSet::load(Some(git_dir), false)
        .ok()
        .map(|c| config_bool_any(&c, &["maintenance.auto"], true))
        .unwrap_or(true);
    if !maintenance_auto {
        return;
    }
    let _ = Command::new(grit_exe::grit_executable())
        .args(["maintenance", "run", "--auto"])
        .env("GIT_DIR", git_dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}
