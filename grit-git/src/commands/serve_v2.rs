//! `grit serve-v2` — protocol v2 server.
//!
//! Implements the server side of Git protocol v2 for testing.
//! Supports capability advertisement, ls-refs, fetch, object-info,
//! and bundle-uri commands.

use anyhow::{bail, Context, Result};
use clap::Args as ClapArgs;
use grit_lib::config::{ConfigFile, ConfigScope, ConfigSet};
use grit_lib::git_date::parse::parse_date_basic;
use grit_lib::merge_base;
use grit_lib::objects::{self, parse_commit, parse_tag, parse_tree, ObjectId, ObjectKind};
use grit_lib::refs;
use grit_lib::repo::Repository;
use grit_lib::rev_parse::resolve_revision;
use std::collections::HashSet;
use std::io::{self, Read, Write};
use std::path::Path;

use grit_lib::pkt_line;

/// Arguments for `grit serve-v2`.
#[derive(Debug, ClapArgs)]
#[command(about = "Protocol v2 server (test helper)")]
pub struct Args {
    /// Advertise capabilities and exit.
    #[arg(long)]
    pub advertise_capabilities: bool,

    /// Stateless RPC mode: read one request from stdin, respond, exit.
    #[arg(long)]
    pub stateless_rpc: bool,
}

/// Known commands and their feature strings.
pub struct ServerCaps {
    agent: String,
    object_format: String,
    advertise_filter: bool,
    advertise_packfile_uris: bool,
    advertise_ref_in_want: bool,
    advertise_object_info: bool,
    advertise_bundle_uri: bool,
    advertise_session_id: bool,
    session_id_value: String,
    /// Value of the `promisor-remote=<info>` capability to advertise, if any (`promisor.advertise`).
    promisor_remote_info: Option<String>,
}

impl ServerCaps {
    /// Load advertised capabilities from repository config at `git_dir`.
    pub fn load(git_dir: &Path) -> Self {
        let agent = serve_agent_capability();

        let object_format = read_object_format(git_dir);

        let promisor_remote_info = grit_lib::config::ConfigSet::load(Some(git_dir), true)
            .ok()
            .and_then(|cfg| grit_lib::promisor_remote::promisor_remote_info(&cfg));

        let advertise_object_info = read_config_bool(git_dir, "transfer.advertiseObjectInfo");
        let advertise_bundle_uri = read_config_bool(git_dir, "uploadpack.advertiseBundleURIs");
        let advertise_filter = read_config_bool(git_dir, "uploadpack.allowfilter");
        let advertise_packfile_uris = read_config_nonempty(git_dir, "uploadpack.blobpackfileuri");
        let advertise_ref_in_want = read_config_bool(git_dir, "uploadpack.allowrefinwant");
        let advertise_session_id = read_config_bool(git_dir, "transfer.advertiseSID")
            || read_config_bool(git_dir, "transfer.advertisesid")
            || read_config_bool(git_dir, "transfer.advertiseSid");
        let session_id_value = if advertise_session_id {
            crate::trace2_transfer::trace2_session_id_wire_once()
        } else {
            String::new()
        };

        Self {
            agent,
            object_format,
            advertise_filter,
            advertise_packfile_uris,
            advertise_ref_in_want,
            advertise_object_info,
            advertise_bundle_uri,
            advertise_session_id,
            session_id_value,
            promisor_remote_info,
        }
    }

    /// Write the capability advertisement to `w` in pkt-line format.
    pub fn advertise(&self, w: &mut impl Write) -> io::Result<()> {
        pkt_line::write_line(w, "version 2")?;
        pkt_line::write_line(w, &self.agent)?;
        pkt_line::write_line(w, "ls-refs=unborn")?;
        let mut fetch_features = String::from("fetch=shallow wait-for-done");
        if self.advertise_filter {
            fetch_features.push_str(" filter");
        }
        if self.advertise_packfile_uris {
            fetch_features.push_str(" packfile-uris");
        }
        if self.advertise_ref_in_want {
            fetch_features.push_str(" ref-in-want");
        }
        pkt_line::write_line(w, &fetch_features)?;
        pkt_line::write_line(w, "server-option")?;
        pkt_line::write_line(w, &format!("object-format={}", self.object_format))?;
        if self.advertise_object_info {
            pkt_line::write_line(w, "object-info")?;
        }
        if self.advertise_bundle_uri {
            pkt_line::write_line(w, "bundle-uri")?;
        }
        if self.advertise_session_id {
            pkt_line::write_line(w, &format!("session-id={}", self.session_id_value))?;
        }
        if let Some(info) = &self.promisor_remote_info {
            pkt_line::write_line(w, &format!("promisor-remote={info}"))?;
        }
        pkt_line::write_flush(w)?;
        w.flush()
    }

    pub fn is_valid_command(&self, cmd: &str) -> bool {
        match cmd {
            "ls-refs" | "fetch" => true,
            "object-info" if self.advertise_object_info => true,
            "bundle-uri" if self.advertise_bundle_uri => true,
            _ => false,
        }
    }

    pub fn is_valid_capability(&self, cap: &str) -> bool {
        // Capabilities that may appear in a request
        cap.starts_with("agent=")
            || cap.starts_with("object-format=")
            || cap.starts_with("server-option=")
            || cap.starts_with("session-id=")
    }
}

fn serve_agent_capability() -> String {
    if let Ok(value) = std::env::var("GIT_USER_AGENT") {
        if !value.trim().is_empty() {
            return format!("agent={value}");
        }
    }
    format!(
        "agent=git/{}-{}",
        crate::version_string(),
        serve_agent_platform()
    )
}

fn serve_agent_platform() -> &'static str {
    match std::env::consts::OS {
        "linux" => "Linux",
        "macos" => "Darwin",
        "windows" => "Windows",
        "freebsd" => "FreeBSD",
        "openbsd" => "OpenBSD",
        "netbsd" => "NetBSD",
        "dragonfly" => "DragonFly",
        "solaris" => "SunOS",
        other => other,
    }
}

pub fn run(args: Args) -> Result<()> {
    let git_dir = discover_git_dir()?;
    let caps = ServerCaps::load(&git_dir);

    if args.advertise_capabilities {
        let stdout = io::stdout();
        let mut out = stdout.lock();
        caps.advertise(&mut out)?;
        return Ok(());
    }

    if args.stateless_rpc {
        let _ = process_one_v2_request(&mut io::stdin().lock(), &git_dir, &caps, true)?;
        return Ok(());
    }

    // Default: advertise + serve loop (matches `git serve-v2` / upload-pack v2).
    let stdout = io::stdout();
    let mut out = stdout.lock();
    caps.advertise(&mut out)?;
    drop(out);
    serve_loop(&mut io::stdin().lock(), &git_dir, &caps)
}

/// Read requests from `input` until EOF or a headerless flush (client hang-up).
///
/// This is the stateful (file/ssh local) transport: a single persistent connection carries the
/// whole exchange, so once the server is `ready` it streams the packfile in the same response
/// without reading a follow-up request.
pub fn serve_loop(input: &mut impl Read, git_dir: &Path, caps: &ServerCaps) -> Result<()> {
    loop {
        if process_one_v2_request(input, git_dir, caps, false)? {
            break;
        }
    }
    Ok(())
}

/// Process a single protocol v2 request from `input`.
///
/// `stateless_rpc` is true for the smart-HTTP transport, where each negotiation round is a separate
/// request/response: when the server becomes `ready` it ends the response after the
/// `acknowledgments` section and the client sends a follow-up request (with `done`) to receive the
/// pack. In stateful mode (`false`) the server instead falls through to stream the pack inline.
///
/// Returns `Ok(true)` when the client ended the session (EOF or flush with no keys).
pub fn process_one_v2_request(
    input: &mut impl Read,
    git_dir: &Path,
    caps: &ServerCaps,
    stateless_rpc: bool,
) -> Result<bool> {
    let (header_lines, terminator) = pkt_line::read_until_flush_or_delim(input)?;

    if header_lines.is_empty() {
        return Ok(matches!(terminator, Some(pkt_line::Packet::Flush) | None));
    }

    let mut command: Option<String> = None;
    let mut client_object_format: Option<String> = None;
    let mut client_session_id: Option<String> = None;
    let mut accepted_promisor_remotes: Option<String> = None;

    for line in &header_lines {
        if let Some(cmd) = line.strip_prefix("command=") {
            if cmd.contains('=') {
                bail!("invalid command '{cmd}'");
            }
            command = Some(cmd.to_owned());
        } else if let Some(fmt) = line.strip_prefix("object-format=") {
            client_object_format = Some(fmt.to_owned());
        } else if let Some(sid) = line.strip_prefix("session-id=") {
            client_session_id = Some(sid.to_owned());
        } else if let Some(remotes) = line.strip_prefix("promisor-remote=") {
            // The client accepted these advertised promisor remotes; it will lazily fetch the
            // omitted objects from them, so the server must NOT back-fill and serve them.
            accepted_promisor_remotes = Some(remotes.to_owned());
        } else if caps.is_valid_capability(line) {
        } else {
            bail!("unknown capability '{line}'");
        }
    }

    let cmd = match command {
        Some(c) => c,
        None => bail!("no command requested"),
    };

    if let Some(ref fmt) = client_object_format {
        if fmt != &caps.object_format {
            bail!(
                "mismatched object format: client={fmt}, server={}",
                caps.object_format
            );
        }
    }

    if !caps.is_valid_command(&cmd) {
        eprintln!("fatal: invalid command '{cmd}'");
        std::process::exit(128);
    }

    if matches!(cmd.as_str(), "ls-refs" | "fetch") {
        if let Some(ref sid) = client_session_id {
            crate::trace2_transfer::emit_client_sid(sid);
        }
    }

    let flush_err = match cmd.as_str() {
        "ls-refs" => "expected flush after ls-refs arguments",
        "fetch" => "expected flush after fetch arguments",
        "object-info" => "object-info: expected flush after arguments",
        "bundle-uri" => "bundle-uri: expected flush after arguments",
        _ => "expected flush after command arguments",
    };

    let args = if terminator == Some(pkt_line::Packet::Delim) {
        pkt_line::read_data_lines_until_flush(input, flush_err).map_err(anyhow::Error::from)?
    } else {
        Vec::new()
    };

    let stdout = io::stdout();
    let mut out = stdout.lock();

    match cmd.as_str() {
        "ls-refs" => cmd_ls_refs(git_dir, &args, &mut out)?,
        "fetch" => cmd_fetch(
            git_dir,
            &args,
            &mut out,
            caps,
            accepted_promisor_remotes.as_deref(),
            stateless_rpc,
        )?,
        "object-info" => cmd_object_info(git_dir, &args, &mut out)?,
        "bundle-uri" => cmd_bundle_uri(git_dir, &args, &mut out)?,
        _ => bail!("invalid command '{cmd}'"),
    }

    out.flush()?;
    Ok(false)
}

/// Handle the `ls-refs` command.
fn cmd_ls_refs(git_dir: &Path, args: &[String], out: &mut impl Write) -> Result<()> {
    let mut prefixes: Vec<String> = Vec::new();
    let mut peel = false;
    let mut symrefs = false;
    let mut unborn = false;

    for arg in args {
        if let Some(prefix) = arg.strip_prefix("ref-prefix ") {
            prefixes.push(prefix.to_owned());
        } else if arg == "peel" {
            peel = true;
        } else if arg == "symrefs" {
            symrefs = true;
        } else if arg == "unborn" {
            // The `unborn` feature (advertised as `ls-refs=unborn`) asks the server to report HEAD
            // even when it points at an unborn branch (empty repository). `lsrefs.unborn` defaults
            // to "advertise", so honour the request unconditionally.
            unborn = true;
        } else {
            bail!("unexpected line: '{arg}'");
        }
    }

    // If too many prefixes (>= 65536), ignore them all (list everything).
    let use_prefixes = prefixes.len() < 65536;

    // `transfer.hideRefs` / `uploadpack.hideRefs`: hidden refs are not advertised. Patterns prefixed
    // `^` match the full storage name, otherwise the namespace-stripped advertised name.
    let cfg = ConfigSet::load(Some(git_dir), false).unwrap_or_default();
    let hide = grit_lib::hide_refs::hide_ref_patterns_uploadpack(&cfg);

    // Collect all refs.
    let mut entries: Vec<RefInfo> = Vec::new();

    // HEAD (resolved relative to the active namespace). Its symref-target is the namespace-stripped
    // logical ref so a clone under `GIT_NAMESPACE` selects the namespaced HEAD branch (t5509).
    if let Ok(head_oid) = refs::resolve_ref(git_dir, "HEAD") {
        let symref_target = if symrefs {
            refs::read_symbolic_ref(git_dir, "HEAD")
                .ok()
                .flatten()
                .map(|t| grit_lib::ref_namespace::strip_namespace_prefix(&t).into_owned())
        } else {
            None
        };
        entries.push(RefInfo {
            name: "HEAD".to_owned(),
            oid: Some(head_oid),
            symref_target,
            peeled: None,
        });
    } else if unborn && symrefs {
        // HEAD points at an unborn branch (empty repository): there is no OID, but when both the
        // `unborn` and `symrefs` features were requested we still report it as
        // `unborn HEAD symref-target:<target>` so the client can discover the default branch and,
        // crucially, the negotiated object format for an empty SHA-256 clone (`t5551`,
        // mirroring upstream `send_possibly_unborn_head`).
        if let Ok(Some(target)) = refs::read_symbolic_ref(git_dir, "HEAD") {
            let symref_target =
                grit_lib::ref_namespace::strip_namespace_prefix(&target).into_owned();
            entries.push(RefInfo {
                name: "HEAD".to_owned(),
                oid: None,
                symref_target: Some(symref_target),
                peeled: None,
            });
        }
    }

    // All refs under refs/ (logical/stripped names when a namespace is active).
    for prefix in &["refs/heads/", "refs/tags/", "refs/remotes/", "refs/notes/"] {
        if let Ok(ref_list) = refs::list_refs(git_dir, prefix) {
            for (name, oid) in ref_list {
                let full = grit_lib::ref_namespace::storage_ref_name(&name);
                if grit_lib::hide_refs::ref_is_hidden(&name, &full, &hide) {
                    continue;
                }
                let mut info = RefInfo {
                    name: name.clone(),
                    oid: Some(oid),
                    symref_target: None,
                    peeled: None,
                };
                if symrefs {
                    info.symref_target = refs::read_symbolic_ref(git_dir, &name)
                        .ok()
                        .flatten()
                        .map(|t| grit_lib::ref_namespace::strip_namespace_prefix(&t).into_owned());
                }
                if peel && name.starts_with("refs/tags/") {
                    info.peeled = peel_to_commit(git_dir, &oid);
                }
                entries.push(info);
            }
        }
    }

    // Filter by prefix
    if use_prefixes && !prefixes.is_empty() {
        entries.retain(|e| prefixes.iter().any(|p| e.name.starts_with(p)));
    }

    // Sort by ref name
    entries.sort_by(|a, b| a.name.cmp(&b.name));

    // Write output. An unborn HEAD has no OID and is emitted as the literal `unborn` token
    // (gitprotocol-v2 `obj-id-or-unborn`).
    for entry in &entries {
        let oid_field = match entry.oid {
            Some(oid) => oid.to_hex(),
            None => "unborn".to_owned(),
        };
        let mut line = format!("{oid_field} {}", entry.name);
        if let Some(ref peeled) = entry.peeled {
            line.push_str(&format!(" peeled:{}", peeled.to_hex()));
        }
        if let Some(ref target) = entry.symref_target {
            line.push_str(&format!(" symref-target:{target}"));
        }
        pkt_line::write_line(out, &line)?;
    }
    pkt_line::write_flush(out)?;
    Ok(())
}

struct RefInfo {
    name: String,
    /// `None` marks an unborn HEAD (empty repository), emitted as the literal `unborn` token.
    oid: Option<grit_lib::objects::ObjectId>,
    symref_target: Option<String>,
    peeled: Option<grit_lib::objects::ObjectId>,
}

/// Peel a tag to its target object. Returns None if not an annotated tag.
fn peel_to_commit(
    git_dir: &Path,
    oid: &grit_lib::objects::ObjectId,
) -> Option<grit_lib::objects::ObjectId> {
    let repo = Repository::open(git_dir, None).ok()?;
    let obj = repo.odb.read(oid).ok()?;
    if obj.kind == ObjectKind::Tag {
        let tag = objects::parse_tag(&obj.data).ok()?;
        Some(tag.object)
    } else {
        None
    }
}

/// Handle the `fetch` command (protocol v2): negotiation + `packfile` section with raw pack bytes.
fn cmd_fetch(
    git_dir: &Path,
    args: &[String],
    out: &mut impl Write,
    caps: &ServerCaps,
    accepted_promisor_remotes: Option<&str>,
    stateless_rpc: bool,
) -> Result<()> {
    let repo = Repository::open(git_dir, None)
        .with_context(|| format!("could not open repository at '{}'", git_dir.display()))?;
    let config = ConfigSet::load(Some(git_dir), true).unwrap_or_default();
    grit_lib::upload_filter::validate_upload_filter_config(&config)?;

    let hide_ref_patterns = grit_lib::hide_refs::hide_ref_patterns_uploadpack(&config);

    let mut wants: Vec<ObjectId> = Vec::new();
    // `want-ref` lines, in first-seen order, resolved to `(logical refname, oid)`.
    // Emitted back to the client as the `wanted-refs` section (matches
    // `upload-pack.c` `send_wanted_ref_info`).
    let mut wanted_refs: Vec<(String, ObjectId)> = Vec::new();
    let mut wanted_ref_names: HashSet<String> = HashSet::new();
    let mut have_oids: Vec<ObjectId> = Vec::new();
    let mut client_shallow_oids: HashSet<ObjectId> = HashSet::new();
    let mut depth_request: Option<usize> = None;
    let mut deepen_since: Option<i64> = None;
    let mut deepen_not: Vec<ObjectId> = Vec::new();
    let mut deepen_relative = false;
    let mut filter_spec: Option<String> = None;
    let mut wait_for_done = false;
    let mut seen_done = false;
    let mut no_progress = false;

    for arg in args {
        match arg.as_str() {
            "no-progress" => no_progress = true,
            "thin-pack" | "include-tag" | "ofs-delta" => {}
            "wait-for-done" => wait_for_done = true,
            "done" => seen_done = true,
            "deepen-relative" => deepen_relative = true,
            s if s.starts_with("want ") => {
                let rest = s.strip_prefix("want ").unwrap_or("").trim();
                let hex = rest.split_whitespace().next().unwrap_or(rest);
                wants.push(
                    ObjectId::from_hex(hex).with_context(|| format!("invalid want oid: {hex}"))?,
                );
                let feats = rest.strip_prefix(hex).unwrap_or("").trim();
                if let Some(sid) = crate::trace2_transfer::extract_session_id_feature(feats) {
                    crate::trace2_transfer::emit_client_sid(sid);
                }
            }
            s if s.starts_with("have ") => {
                let hex = s.strip_prefix("have ").unwrap_or("").trim();
                if let Ok(oid) = ObjectId::from_hex(hex) {
                    have_oids.push(oid);
                }
            }
            s if s.starts_with("deepen ") => {
                let depth_text = s.strip_prefix("deepen ").unwrap_or("").trim();
                if !depth_text.is_empty() {
                    if let Ok(depth) = depth_text.parse::<usize>() {
                        if depth > 0 && depth < i32::MAX as usize {
                            depth_request = Some(depth);
                        }
                    }
                }
            }
            s if s.starts_with("shallow ") => {
                let hex = s.strip_prefix("shallow ").unwrap_or("").trim();
                if let Ok(oid) = ObjectId::from_hex(hex) {
                    client_shallow_oids.insert(oid);
                }
            }
            s if s.starts_with("deepen-since ") => {
                let date = s.strip_prefix("deepen-since ").unwrap_or("").trim();
                if let Ok((timestamp, _)) = parse_date_basic(date) {
                    deepen_since = Some(timestamp as i64);
                } else if let Ok(timestamp) = date.parse::<i64>() {
                    deepen_since = Some(timestamp);
                }
            }
            s if s.starts_with("deepen-not ") => {
                let rev = s.strip_prefix("deepen-not ").unwrap_or("").trim();
                if let Ok(oid) = ObjectId::from_hex(rev).or_else(|_| resolve_revision(&repo, rev)) {
                    deepen_not.push(oid);
                }
            }
            s if s.starts_with("want-ref ") => {
                if !caps.advertise_ref_in_want {
                    bail!("unexpected line: '{s}'");
                }
                let refname = s.strip_prefix("want-ref ").unwrap_or("").trim().to_owned();
                // Resolve the ref relative to the active namespace and reject it when
                // hidden or absent (matches `upload-pack.c` `parse_want_ref`, which
                // forms `<namespace><refname>` then checks `ref_is_hidden` and
                // `refs_read_ref`). On failure the server writes a pkt-line error and
                // dies, which the client surfaces as "unknown ref <refname>".
                let storage = grit_lib::ref_namespace::storage_ref_name(&refname);
                let hidden =
                    grit_lib::hide_refs::ref_is_hidden(&refname, &storage, &hide_ref_patterns);
                // Resolve the *storage* refname exactly (no DWIM, no fallback to the
                // non-namespaced name). `parse_want_ref` reads `<namespace><refname>`
                // directly, so `want-ref refs/heads/ns-no` under `GIT_NAMESPACE=ns` must
                // not silently fall back to a top-level `refs/heads/ns-no` (t5703 "with
                // namespace: want-ref outside namespace is unknown").
                let resolved = if hidden {
                    None
                } else {
                    refs::resolve_ref(git_dir, &storage).ok()
                };
                let oid = match resolved {
                    Some(oid) => oid,
                    None => {
                        pkt_line::write_line(out, &format!("ERR unknown ref {refname}"))?;
                        out.flush()?;
                        eprintln!("fatal: unknown ref {refname}");
                        std::process::exit(128);
                    }
                };
                if !wanted_ref_names.insert(refname.clone()) {
                    pkt_line::write_line(out, &format!("ERR duplicate want-ref {refname}"))?;
                    out.flush()?;
                    eprintln!("fatal: duplicate want-ref {refname}");
                    std::process::exit(128);
                }
                wanted_refs.push((refname, oid));
                wants.push(oid);
            }
            s if s.starts_with("filter ") => {
                if !caps.advertise_filter {
                    bail!("unexpected line: '{s}'");
                }
                let spec = s.strip_prefix("filter ").unwrap_or("").trim();
                if spec.is_empty() {
                    bail!("unexpected line: '{s}'");
                }
                grit_lib::upload_filter::validate_upload_filter_request(&config, spec)?;
                filter_spec = Some(spec.to_owned());
            }
            s if s.starts_with("packfile-uris ") => {
                if !caps.advertise_packfile_uris {
                    bail!("unexpected line: '{s}'");
                }
            }
            s if s.starts_with("sideband-all") => {}
            other => bail!("unexpected line: '{other}'"),
        }
    }

    // Validate every `want <oid>` line before serving. In protocol v2, `upload-pack.c`'s `parse_want`
    // only verifies that the wanted object is *present* locally (`parse_object_with_flags` returns
    // non-NULL); it does NOT apply the v0 `check_non_tip` / `allow{Tip,Reachable,Any}SHA1InWant`
    // gating. The non-tip rejection is explicitly a v0-only behavior, so a v2 client may legitimately
    // `want` any object the server holds — e.g. a shallow boundary commit that is not a ref tip
    // (t5537 `fetch --update-shallow <shallow-point>:refs/heads/...`). A `want` for an object we do
    // not have still draws an `ERR upload-pack: not our ref <oid>` packet and a fatal exit, which is
    // what makes a fetch fail when an advertised ref changed under the client mid-negotiation (t5703
    // change-while-negotiating). `want-ref` OIDs always resolve to one of our refs, so they pass here.
    for w in &wants {
        if repo.odb.read(w).is_err() {
            // Object not present at all — never our ref.
            return serve_reject_not_our_ref(out, w);
        }
    }

    if wants.is_empty() && !wait_for_done {
        pkt_line::write_flush(out)?;
        return Ok(());
    }

    let want_set: HashSet<ObjectId> = wants.iter().copied().collect();
    let mut have_commits: Vec<ObjectId> = Vec::new();
    // `acknowledgments` ACKs every `have` the server already has, de-duplicated and in first-seen
    // order (matching `upload-pack.c` `do_got_oid`: each object's `THEY_HAVE` flag is set once, so a
    // repeated `have` for the same object is ACKed only once). Non-commit `have` objects (trees,
    // blobs) are ACKed too — they just do not contribute ancestor history.
    let mut acks: Vec<ObjectId> = Vec::new();
    let mut acked: HashSet<ObjectId> = HashSet::new();
    for h in &have_oids {
        if let Ok(obj) = repo.odb.read(h) {
            if obj.kind == ObjectKind::Commit {
                have_commits.push(*h);
            }
            if acked.insert(*h) {
                acks.push(*h);
            }
        }
    }

    if !have_oids.is_empty() && !seen_done {
        pkt_line::write_line(out, "acknowledgments")?;
        if acks.is_empty() {
            pkt_line::write_line(out, "NAK")?;
        }
        for oid in &acks {
            pkt_line::write_line(out, &format!("ACK {}", oid.to_hex()))?;
        }
        if ok_to_give_up_v2(&repo, &want_set, &have_commits) {
            pkt_line::write_line(out, "ready")?;
            pkt_line::write_delim(out)?;
            if stateless_rpc {
                // Smart-HTTP: end the response after `ready`; the client sends a follow-up request
                // (with `done`) to receive the pack.
                out.flush()?;
                return Ok(());
            }
            // Stateful transport: the `acknowledgments` section is followed in the SAME response by
            // the `packfile` section. Matches `upload-pack.c`'s state machine, where
            // `UPLOAD_SEND_ACKS` transitions to `UPLOAD_SEND_PACK` without reading another request.
            // A stateful client does not send a follow-up after `ready`, so returning here would
            // deadlock the connection. Fall through to send shallow-info / wanted-refs / packfile.
        } else {
            // Not ready yet: end the round with a flush so the client sends more haves.
            pkt_line::write_flush(out)?;
            return Ok(());
        }
    }

    // `shallow-info` then `wanted-refs` before `packfile` (gitprotocol-v2 and `upload-pack.c`
    // after rr/upload-pack-swap-shallow-wanted-ref). Clients parse sections in this order.
    let client_shallow_vec = client_shallow_oids.iter().copied().collect::<Vec<_>>();
    let mut new_shallow = Vec::new();
    if let Some(depth) = depth_request {
        new_shallow = grit_lib::rev_list::shallow_grafts_for_upload_pack_deepen(
            &repo,
            &wants,
            &client_shallow_vec,
            depth,
        );
    } else if deepen_since.is_some() || !deepen_not.is_empty() {
        new_shallow = grit_lib::rev_list::shallow_grafts_for_upload_pack_rev_list(
            &repo,
            &wants,
            &client_shallow_vec,
            deepen_since,
            &deepen_not,
        )?;
    }
    if depth_request.is_some() || deepen_since.is_some() || !deepen_not.is_empty() {
        let new_shallow_set: HashSet<ObjectId> = new_shallow.iter().copied().collect();
        pkt_line::write_line(
            out,
            grit_lib::protocol_v2::FetchResponseSection::ShallowInfo.header(),
        )?;
        for oid in &new_shallow {
            pkt_line::write_line(out, &format!("shallow {}", oid.to_hex()))?;
        }
        for oid in &client_shallow_vec {
            if !new_shallow_set.contains(oid) {
                pkt_line::write_line(out, &format!("unshallow {}", oid.to_hex()))?;
            }
        }
        pkt_line::write_delim(out)?;
    }

    // `wanted-refs` resolves the client's `want-ref` requests to concrete OIDs.
    if !wanted_refs.is_empty() {
        pkt_line::write_line(
            out,
            grit_lib::protocol_v2::FetchResponseSection::WantedRefs.header(),
        )?;
        for (refname, oid) in &wanted_refs {
            pkt_line::write_line(out, &format!("{} {}", oid.to_hex(), refname))?;
        }
        pkt_line::write_delim(out)?;
    }

    pkt_line::write_line(out, "packfile")?;
    let thin = !have_oids.is_empty() && client_shallow_oids.is_empty();
    // For a depth-limited request, cut the boundary commits' parent chains with `--shallow <oid>`
    // instead of excluding the boundary's parents with `--not`. Excluding the parents would also
    // drop trees/blobs shared between the in-depth commits and the cut-off history (e.g. a file
    // added in the first commit and never modified), producing a shallow pack whose refs reference
    // missing objects — caught by `git fsck` in t5537 "shallow fetches check connectivity before
    // writing shallow file".
    let shallow_commits: Vec<ObjectId> = if let Some(mut depth) = depth_request {
        if deepen_relative && !client_shallow_oids.is_empty() {
            let base =
                relative_depth_base_from_client_shallows(&repo, &wants, &client_shallow_oids);
            depth = depth.saturating_add(base);
        }
        crate::pack_objects_upload::compute_depth_boundary_commits(&repo, &wants, depth)?
    } else {
        Vec::new()
    };
    // The client accepted one or more advertised promisor remotes: it will lazily fetch any
    // omitted objects from them, so the server may omit locally-missing promisor objects from the
    // filtered pack instead of back-filling its ODB to serve them.
    let accepted_promisor = accepted_promisor_remotes
        .as_deref()
        .map(|r| !r.trim().is_empty())
        .unwrap_or(false);
    let omit_missing_promisor = accepted_promisor && filter_spec.is_some();
    // `upload-pack` pins `GIT_NO_LAZY_FETCH=1` by default, so the server-side `pack-objects` never
    // lazily fetches missing objects from a promisor remote — it just fails if it cannot read an
    // object (t0411-clone-from-partial: a plain clone/fetch from a partial-clone server must NOT
    // run the promisor's upload-pack). Only when the operator explicitly re-enables lazy fetching
    // (`GIT_NO_LAZY_FETCH=0`) may the server back-fill omitted blobs before serving them.
    let lazy_fetch_enabled =
        !crate::commands::promisor_hydrate::git_no_lazy_fetch_env_disables_lazy().unwrap_or(false);
    let force_lazy_fetch = lazy_fetch_enabled
        && if filter_spec.is_some() {
            !omit_missing_promisor
        } else {
            caps.promisor_remote_info.is_none()
        };
    let mut exclude_commits = if client_shallow_oids.is_empty() {
        have_commits.clone()
    } else {
        Vec::new()
    };
    exclude_commits.sort_by_key(|oid| oid.to_hex());
    exclude_commits.dedup();
    if force_lazy_fetch && !exclude_commits.is_empty() {
        hydrate_upload_pack_blobs_missing_from_client(&repo, &wants, &exclude_commits)?;
    }
    let mut child = crate::pack_objects_upload::spawn_pack_objects_upload_shallow(
        git_dir,
        thin,
        filter_spec.as_deref(),
        !shallow_commits.is_empty(),
        !no_progress,
        omit_missing_promisor,
        force_lazy_fetch,
    )?;
    {
        let mut pin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow::anyhow!("pack-objects stdin"))?;
        crate::pack_objects_upload::write_pack_objects_revs_stdin_shallow(
            &mut pin,
            &wants,
            &exclude_commits,
            &shallow_commits,
        )?;
    }
    // Protocol v2 fetch streams the pack inside side-band-64k (matches `git upload-pack`).
    crate::pack_objects_upload::drain_pack_objects_child(child, out, true)?;
    pkt_line::write_flush(out)?;
    Ok(())
}

/// Send `ERR upload-pack: not our ref <oid>` and exit 128 — matching `upload-pack.c` rejection of a
/// `want` for an object we cannot serve. The client surfaces this as
/// `fatal: remote error: upload-pack: not our ref`.
fn serve_reject_not_our_ref(out: &mut impl Write, oid: &ObjectId) -> Result<()> {
    let hex = oid.to_hex();
    pkt_line::write_line(out, &format!("ERR upload-pack: not our ref {hex}"))?;
    out.flush()?;
    eprintln!("error: git upload-pack: not our ref {hex}");
    std::process::exit(128);
}

fn ok_to_give_up_v2(
    repo: &Repository,
    wants: &HashSet<ObjectId>,
    have_commits: &[ObjectId],
) -> bool {
    if have_commits.is_empty() {
        return false;
    }
    let mut client_known: HashSet<ObjectId> = HashSet::new();
    for h in have_commits {
        if merge_ancestors_into_v2(repo, *h, &mut client_known).is_err() {
            return false;
        }
    }
    wants.iter().all(|w| {
        client_known
            .iter()
            .any(|h| merge_base::is_ancestor(repo, *h, *w).unwrap_or(false))
    })
}

fn hydrate_upload_pack_blobs_missing_from_client(
    repo: &Repository,
    wants: &[ObjectId],
    exclusions: &[ObjectId],
) -> Result<()> {
    let mut needed = reachable_blob_oids(repo, wants);
    for oid in reachable_blob_oids(repo, exclusions) {
        needed.remove(&oid);
    }
    let mut missing: Vec<ObjectId> = needed
        .into_iter()
        .filter(|oid| repo.odb.read(oid).is_err())
        .collect();
    if missing.is_empty() {
        return Ok(());
    }
    missing.sort();
    let previous = std::env::var_os("GIT_NO_LAZY_FETCH");
    std::env::set_var("GIT_NO_LAZY_FETCH", "0");
    let result =
        crate::commands::promisor_hydrate::try_lazy_fetch_promisor_objects_batch(repo, &missing);
    if let Some(value) = previous {
        std::env::set_var("GIT_NO_LAZY_FETCH", value);
    } else {
        std::env::remove_var("GIT_NO_LAZY_FETCH");
    }
    result
}

fn reachable_blob_oids(repo: &Repository, roots: &[ObjectId]) -> HashSet<ObjectId> {
    let mut blobs = HashSet::new();
    let mut seen = HashSet::new();
    let mut stack: Vec<ObjectId> = roots.to_vec();
    while let Some(oid) = stack.pop() {
        if !seen.insert(oid) {
            continue;
        }
        let Ok(obj) = repo.odb.read(&oid) else {
            continue;
        };
        match obj.kind {
            ObjectKind::Commit => {
                if let Ok(commit) = parse_commit(&obj.data) {
                    stack.push(commit.tree);
                    stack.extend(commit.parents);
                }
            }
            ObjectKind::Tree => {
                if let Ok(entries) = parse_tree(&obj.data) {
                    for entry in entries {
                        match entry.mode {
                            0o040000 => stack.push(entry.oid),
                            0o160000 => {}
                            _ => {
                                blobs.insert(entry.oid);
                            }
                        }
                    }
                }
            }
            ObjectKind::Tag => {
                if let Ok(tag) = parse_tag(&obj.data) {
                    stack.push(tag.object);
                }
            }
            ObjectKind::Blob => {
                blobs.insert(oid);
            }
        }
    }
    blobs
}

fn merge_ancestors_into_v2(
    repo: &Repository,
    tip: ObjectId,
    into: &mut HashSet<ObjectId>,
) -> anyhow::Result<()> {
    let anc = merge_base::ancestor_closure(repo, tip)?;
    into.extend(anc);
    Ok(())
}

fn relative_depth_base_from_client_shallows(
    repo: &Repository,
    wants: &[ObjectId],
    client_shallow_oids: &HashSet<ObjectId>,
) -> usize {
    wants
        .iter()
        .filter_map(|want| shortest_depth_to_boundary(repo, *want, client_shallow_oids))
        .max()
        .unwrap_or(0)
}

fn shortest_depth_to_boundary(
    repo: &Repository,
    start: ObjectId,
    boundaries: &HashSet<ObjectId>,
) -> Option<usize> {
    let mut queue = std::collections::VecDeque::new();
    let mut seen = HashSet::new();
    queue.push_back((start, 1usize));
    while let Some((oid, depth)) = queue.pop_front() {
        if !seen.insert(oid) {
            continue;
        }
        if boundaries.contains(&oid) {
            return Some(depth);
        }
        let obj = repo.odb.read(&oid).ok()?;
        if obj.kind != ObjectKind::Commit {
            continue;
        }
        let commit = parse_commit(&obj.data).ok()?;
        for parent in commit.parents {
            queue.push_back((parent, depth + 1));
        }
    }
    None
}

/// Handle the `object-info` command.
fn cmd_object_info(git_dir: &Path, args: &[String], out: &mut impl Write) -> Result<()> {
    let repo = Repository::open(git_dir, None).with_context(|| "could not open repository")?;

    let mut want_size = false;
    let mut oids: Vec<grit_lib::objects::ObjectId> = Vec::new();

    for arg in args {
        if arg == "size" {
            want_size = true;
        } else if let Some(hex) = arg.strip_prefix("oid ") {
            let oid: grit_lib::objects::ObjectId =
                hex.parse().with_context(|| format!("invalid oid: {hex}"))?;
            oids.push(oid);
        }
    }

    if want_size {
        pkt_line::write_line(out, "size")?;
    }

    for oid in &oids {
        let obj = repo.odb.read(oid)?;
        if want_size {
            pkt_line::write_line(out, &format!("{} {}", oid.to_hex(), obj.data.len()))?;
        }
    }

    pkt_line::write_flush(out)?;
    Ok(())
}

/// Handle the `bundle-uri` command: stream `bundle.*` config as `key=value` pkt-lines.
fn cmd_bundle_uri(git_dir: &Path, args: &[String], out: &mut impl Write) -> Result<()> {
    if !args.is_empty() {
        bail!("bundle-uri: unexpected argument: '{}'", args[0]);
    }
    let path = git_dir.join("config");
    let content =
        std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    let cfg = ConfigFile::parse(&path, &content, ConfigScope::Local)?;
    let mut lines: Vec<(String, String)> = Vec::new();
    for e in &cfg.entries {
        if e.key.starts_with("bundle.") {
            if let Some(v) = e.value.as_deref() {
                lines.push((e.key.clone(), v.to_string()));
            }
        }
    }
    lines.sort_by(|a, b| a.0.cmp(&b.0));
    for (k, v) in lines {
        pkt_line::write_line(out, &format!("{k}={v}"))?;
    }
    pkt_line::write_flush(out)?;
    Ok(())
}

/// Read a boolean config value.
/// Read the repository's object format (`extensions.objectformat`), defaulting to `sha1`.
///
/// The advertised `object-format` capability lets a SHA-256-aware client clone a SHA-256
/// repository (including an empty one) with the correct hash algorithm; otherwise the client
/// assumes the default SHA-1 (`t5551` empty SHA-256 clone over protocol v2).
fn read_object_format(git_dir: &Path) -> String {
    let set = ConfigSet::load(Some(git_dir), false).unwrap_or_default();
    set.get("extensions.objectformat")
        .or_else(|| set.get("extensions.objectFormat"))
        .map(|s| s.to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "sha1".to_owned())
}

fn read_config_bool(git_dir: &Path, key: &str) -> bool {
    // Check environment-based config overrides first
    if let Some(val) = check_env_config(key) {
        return matches!(val.to_lowercase().as_str(), "true" | "yes" | "1");
    }
    if let Some(val) = check_git_config_parameters(key) {
        return matches!(val.to_lowercase().as_str(), "true" | "yes" | "1");
    }
    if let Ok(config) = ConfigSet::load(Some(git_dir), true) {
        if let Some(val) = config.get(key) {
            return matches!(val.to_lowercase().as_str(), "true" | "yes" | "1");
        }
    }
    false
}

fn read_config_nonempty(git_dir: &Path, key: &str) -> bool {
    if let Some(val) = check_env_config(key) {
        return !val.trim().is_empty();
    }
    if let Some(val) = check_git_config_parameters(key) {
        return !val.trim().is_empty();
    }
    if let Ok(config) = ConfigSet::load(Some(git_dir), true) {
        if let Some(val) = config.get(key) {
            return !val.trim().is_empty();
        }
    }
    false
}

/// Check GIT_CONFIG_COUNT/KEY_N/VALUE_N for a given key.
fn check_env_config(key: &str) -> Option<String> {
    let count: usize = std::env::var("GIT_CONFIG_COUNT").ok()?.parse().ok()?;
    for i in 0..count {
        let k = std::env::var(format!("GIT_CONFIG_KEY_{i}")).ok()?;
        if k.eq_ignore_ascii_case(key) {
            return std::env::var(format!("GIT_CONFIG_VALUE_{i}")).ok();
        }
    }
    None
}

fn check_git_config_parameters(key: &str) -> Option<String> {
    let payload = std::env::var("GIT_CONFIG_PARAMETERS").ok()?;
    // Entries are shell-quoted by `apply_globals` as: `'key=value' 'k=v'`.
    // Split on single quotes and inspect odd chunks.
    for entry in payload.split('\'').skip(1).step_by(2) {
        let trimmed = entry.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some((k, v)) = trimmed.split_once('=') {
            if k.eq_ignore_ascii_case(key) {
                return Some(v.to_owned());
            }
        } else if trimmed.eq_ignore_ascii_case(key) {
            return Some(String::new());
        }
    }
    None
}

/// Simple config file parser: find the last value for a key like "section.key"
/// or "section.subsection.key".
fn parse_config_value(contents: &str, key: &str) -> Option<String> {
    let parts: Vec<&str> = key.splitn(2, '.').collect();
    if parts.len() != 2 {
        return None;
    }
    let section = parts[0];
    let var_name = parts[1];

    let mut in_section = false;
    let mut result = None;

    for line in contents.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            // Parse section header
            let header = trimmed.trim_start_matches('[').trim_end_matches(']').trim();
            in_section = header.eq_ignore_ascii_case(section);
        } else if in_section {
            if let Some((k, v)) = trimmed.split_once('=') {
                let k = k.trim();
                let v = v.trim();
                if k.eq_ignore_ascii_case(var_name) {
                    result = Some(v.to_owned());
                }
            }
        }
    }
    result
}

/// Discover the git directory from the current working directory.
fn discover_git_dir() -> Result<std::path::PathBuf> {
    let cwd = std::env::current_dir()?;

    // Check GIT_DIR env
    if let Ok(dir) = std::env::var("GIT_DIR") {
        let p = std::path::Path::new(&dir);
        if p.is_absolute() {
            return Ok(p.to_path_buf());
        }
        return Ok(cwd.join(p));
    }

    // Check if cwd is a bare repo
    if cwd.join("HEAD").exists() && cwd.join("objects").exists() {
        return Ok(cwd.clone());
    }

    // Check .git
    let git_dir = cwd.join(".git");
    if git_dir.is_dir() {
        return Ok(git_dir);
    }
    // .git might be a file (worktree)
    if git_dir.is_file() {
        let contents = std::fs::read_to_string(&git_dir)?;
        if let Some(path) = contents.strip_prefix("gitdir: ") {
            let path = path.trim();
            let p = std::path::Path::new(path);
            if p.is_absolute() {
                return Ok(p.to_path_buf());
            }
            return Ok(cwd.join(p));
        }
    }

    // Walk up
    let mut dir = cwd.as_path();
    loop {
        let candidate = dir.join(".git");
        if candidate.is_dir() {
            return Ok(candidate);
        }
        match dir.parent() {
            Some(p) => dir = p,
            None => bail!("not a git repository (or any parent)"),
        }
    }
}
