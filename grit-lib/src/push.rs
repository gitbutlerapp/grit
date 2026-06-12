//! Wire-protocol push orchestration over a [`crate::transport::Connection`].
//!
//! [`push_remote`] is the wire counterpart to [`crate::transfer::push_local`]:
//! instead of copying objects between two on-disk repositories, it drives a
//! `git-receive-pack` exchange over a live [`crate::transport::Connection`] —
//! reading the receive-pack advertisement (remote refs + `.have` lines +
//! capabilities), deciding each ref update against the advertised remote refs
//! (reusing the same fast-forward / force / force-with-lease rules as
//! `push_local`), building the minimal pack with [`crate::transfer::build_pack`]
//! (using the advertised remote tips + `.have`s as the negotiation `haves`),
//! streaming it, and parsing the `report-status` / `report-status-v2` reply into
//! per-ref [`crate::push_report::PushRefResult`]s.
//!
//! This is the send-pack flow lifted from the CLI's `commands/send_pack.rs`
//! (`run`, `report_has_rejections`, `demux_report_and_remote_messages`),
//! generalized to run over the [`crate::transport::Connection`] reader/writer
//! rather than a spawned `receive-pack` subprocess.
//!
//! Protocol v0/v1 only in this phase (the classic receive-pack advertisement).
//! A protocol-v2 push would require the `command=push` round and is deferred.
//!
//! The wire OID width is the repository's hash algorithm (threaded through
//! [`crate::odb::Odb::hash_algo`]), so SHA-256 repositories push correctly: the
//! zero/null OID, the empty-pack trailer, and the advertisement parsing are all
//! hash-width aware.

use std::collections::HashMap;
use std::collections::HashSet;
use std::io::Cursor;
use std::path::Path;

use crate::error::{Error, Result};
use crate::fetch::Progress;
use crate::objects::{parse_tag, HashAlgo, ObjectId, ObjectKind};
use crate::pkt_line::{self, Packet};
use crate::push_report::{PushRefResult, PushRefStatus};
use crate::transfer::{
    build_pack, open_odb, PackBuildOptions, PushOptions, PushOutcome, PushRefSpec,
};
use crate::transport::Connection;

/// The receive-pack capabilities we negotiate, in the order Git's `send-pack`
/// lists them. `report-status-v2` is requested alongside `report-status` so a
/// modern server can reply with the richer per-ref report; `side-band-64k` lets
/// the server multiplex the report (band 1) and hook/diagnostic output (band 2).
const PUSH_CAPS_BASE: &str = "report-status report-status-v2 quiet";

/// Push refs to a remote over a live [`Connection`] speaking `git-receive-pack`.
///
/// The flow mirrors [`crate::transfer::push_local`], but the remote ref list and
/// `.have` hints come from the connection's advertisement, the objects are
/// streamed over the wire as a single pack, and per-ref acceptance/rejection is
/// learned from the server's `report-status` reply (a server may reject an update
/// our local checks would have accepted, e.g. `denyNonFastForwards` or a
/// pre-receive hook).
///
/// Steps:
/// 1. Read the receive-pack advertisement from `conn`: remote refs (name -> oid),
///    `.have` oids, and capabilities (`report-status(-v2)`, `side-band-64k`,
///    `ofs-delta`, `object-format`).
/// 2. Decide each [`PushRefSpec`] against the advertised remote refs (up-to-date,
///    new, fast-forward, forced, non-fast-forward rejection, force-with-lease
///    stale) — the client-side gate before anything is sent.
/// 3. Write the ref-update commands for the accepted, value-changing updates
///    (`<old> <new> <ref>\0<caps>\n` first, `<old> <new> <ref>\n` rest), then a
///    flush.
/// 4. Build the minimal pack with [`build_pack`] (wants = new tips, haves =
///    advertised remote tips + `.have`s) and stream it; for deletion-only pushes
///    stream the empty pack.
/// 5. Read + parse `report-status` / `report-status-v2` (demultiplexing the
///    side-band if negotiated) and fold the per-ref `ok`/`ng` lines back into the
///    decided results.
///
/// `progress` receives the remote's side-band channel-2 bytes (hook output,
/// `remote: …` diagnostics) when `side-band-64k` is negotiated.
///
/// Protocol v0/v1 only; a v2 connection is rejected.
///
/// # Errors
///
/// Returns an error if the connection is protocol v2, if a source object is
/// missing from the local odb, if the pack build fails, or on wire/parse I/O
/// failure.
pub fn push_remote(
    local_git_dir: &Path,
    conn: &mut dyn Connection,
    refs: &[PushRefSpec],
    opts: &PushOptions,
    progress: &mut dyn Progress,
) -> Result<PushOutcome> {
    use crate::net_trace::net_trace;
    net_trace!(
        "push_remote: begin — {} ref update(s), protocol v{}, {} push-option(s)",
        refs.len(),
        conn.protocol_version(),
        opts.push_options.len()
    );
    if conn.protocol_version() >= 2 {
        return Err(Error::Message(
            "push_remote: protocol v2 not supported in this phase (use v0/v1)".to_owned(),
        ));
    }

    let local_odb = open_odb(local_git_dir);
    let algo = local_odb.hash_algo();

    // 1. Advertisement: split the connection's parsed advertisement into the
    //    remote ref map and the `.have` hints, and read the negotiated caps.
    let adv = AdvertisedState::from_connection(conn);
    net_trace!(
        "push_remote: remote advertised {} ref(s)",
        adv.remote_refs.len()
    );

    // Push-options require the server's `push-options` capability; fail typed
    // (matching Git) before sending anything if the server lacks it.
    require_push_options_supported(&adv, opts)?;

    // 2–3. Decide each ref update client-side, handling atomic/dry-run/no-op
    //       early-returns; the shared planner mirrors `push_local`'s gate.
    let mut plan = match plan_push(refs, &local_odb, local_git_dir, &adv, opts)? {
        PlanOutcome::Send(plan) => plan,
        PlanOutcome::Done(results) => return Ok(PushOutcome { results }),
    };

    // 4. Write the ref-update commands (first carries the cap list after a NUL),
    //    then the pack — but only when there are objects to send. A deletion-only
    //    push streams no pack at all (matching `git send-pack`); `receive-pack`
    //    does not read one after a delete-only command block, so sending an empty
    //    pack would leave unread bytes on the wire and reset the connection.
    let commands = build_command_block(&plan, &adv, algo, &opts.push_options)?;
    net_trace!(
        "push_remote: sending {} command(s)…",
        plan.decisions.len()
    );
    conn.writer().write_all(&commands)?;
    conn.writer().flush()?;

    if let Some(pack) = build_push_pack(&plan, &local_odb, &adv)? {
        net_trace!("push_remote: sending pack ({} bytes)…", pack.len());
        conn.writer().write_all(&pack)?;
        conn.writer().flush()?;
    } else {
        net_trace!("push_remote: no pack (deletion-only / up-to-date)");
    }

    // 5. Read the server's report. With side-band, band 1 carries the
    //    report-status pkt-lines and band 2/3 carry remote diagnostics; without
    //    it the raw stream is the report-status itself.
    //
    // Unlike the v2 *fetch* path, we do NOT half-close the write side before
    // reading: `git-receive-pack` has no persistent v2 serve loop — it consumes
    // the command list and the (length-delimited) pack, writes the report, and
    // exits. It is still reading its input while we read its report, so closing
    // our write half early makes it see a premature EOF ("the remote end hung up
    // unexpectedly") and abort without sending the report. The server closes its
    // own output once the report is written, ending `read_to_end`; the write
    // half is released when the connection is dropped (after the child has
    // already exited, so the `Drop` teardown does not block).
    let mut raw = Vec::new();
    conn.reader().read_to_end(&mut raw)?;
    let report = if adv.server_sideband {
        demux_report_and_remote_messages(&raw, progress)?
    } else {
        raw
    };

    apply_report_status(&report, &mut plan.decisions);

    let results: Vec<_> = plan.decisions.into_iter().map(|d| d.result).collect();
    net_trace!("push_remote: done — {} result(s)", results.len());
    Ok(PushOutcome { results })
}

/// Push refs to a remote over smart HTTP (`git-receive-pack`), returning a
/// [`PushOutcome`].
///
/// This is the stateless-RPC counterpart to [`push_remote`]: instead of a duplex
/// [`Connection`] it issues a `GET info/refs?service=git-receive-pack` discovery
/// (the receive-pack advertisement: remote refs + `.have`s + capabilities), then
/// a single `POST git-receive-pack` whose body is the ref-update commands
/// (`<old> <new> <ref>\0<caps>\n` first, bare after, flush) followed by the
/// packfile; the response is the `report-status` / `report-status-v2`.
///
/// The decision logic, command framing, pack building (thin + delta + advertised
/// `.have` set + `ofs-delta` per caps), and report parsing are the *same* shared
/// helpers used by [`push_remote`] — only the wire (discovery GET + one POST vs.
/// the duplex socket) differs.
///
/// `client` is the embedder's [`crate::transport::http::HttpClient`]; `repo_url`
/// is the remote repository URL (e.g. `http://host/repo.git`). `progress`
/// receives the server's side-band channel-2 bytes (hook output, `remote: …`
/// diagnostics) when `side-band-64k` is negotiated.
///
/// Protocol v0/v1 only; a v2 receive-pack advertisement is rejected (a v2 push
/// would require the `command=push` round and is deferred — matching
/// [`push_remote`]).
///
/// # Errors
///
/// Returns an error if discovery fails, the advertisement is protocol v2, a
/// source object is missing from the local odb, the pack build fails, or on
/// wire/parse I/O failure.
pub fn push_http(
    client: &dyn crate::transport::http::HttpClient,
    local_git_dir: &Path,
    repo_url: &str,
    refs: &[PushRefSpec],
    opts: &PushOptions,
    progress: &mut dyn Progress,
) -> Result<PushOutcome> {
    use crate::net_trace::net_trace;
    net_trace!(
        "push_http: begin — {} ref update(s) to {}, {} push-option(s)",
        refs.len(),
        repo_url,
        opts.push_options.len()
    );
    let local_odb = open_odb(local_git_dir);
    let algo = local_odb.hash_algo();

    // 1. Discovery: GET info/refs?service=git-receive-pack.
    let adv = discover_receive_pack(client, repo_url)?;
    net_trace!(
        "push_http: remote advertised {} ref(s) (protocol v{})",
        adv.state.remote_refs.len(),
        adv.protocol_version
    );
    if adv.protocol_version >= 2 {
        return Err(Error::Message(
            "push_http: protocol v2 receive-pack not supported in this phase (use v0/v1)"
                .to_owned(),
        ));
    }

    // Push-options require the server's `push-options` capability; fail typed
    // (matching Git) before sending anything if the server lacks it.
    require_push_options_supported(&adv.state, opts)?;

    // 2–3. Decide each ref update client-side (shared with `push_remote`).
    let mut plan = match plan_push(refs, &local_odb, local_git_dir, &adv.state, opts)? {
        PlanOutcome::Send(plan) => plan,
        PlanOutcome::Done(results) => return Ok(PushOutcome { results }),
    };

    // 4. Build the single POST body: ref-update commands + flush (then the
    //    push-option lines + flush when negotiated), then the pack (omitted
    //    entirely for a deletion-only push, matching `git send-pack`).
    let mut body = build_command_block(&plan, &adv.state, algo, &opts.push_options)?;
    if let Some(pack) = build_push_pack(&plan, &local_odb, &adv.state)? {
        body.extend_from_slice(&pack);
    }

    // 5. POST git-receive-pack and parse the report-status reply.
    let service_url = receive_pack_url(repo_url);
    let content_type = format!("application/x-{RECEIVE_PACK}-request");
    let accept = format!("application/x-{RECEIVE_PACK}-result");
    net_trace!(
        "push_http: POST git-receive-pack ({} command(s), {} body bytes)…",
        plan.decisions.len(),
        body.len()
    );
    let resp = client.post(&service_url, &content_type, &accept, &body, None)?;

    let report = if adv.state.server_sideband {
        demux_report_and_remote_messages(&resp, progress)?
    } else {
        resp
    };

    apply_report_status(&report, &mut plan.decisions);
    net_trace!(
        "push_http: done — {} result(s)",
        plan.decisions.len()
    );

    Ok(PushOutcome {
        results: plan.decisions.into_iter().map(|d| d.result).collect(),
    })
}

const RECEIVE_PACK: &str = "git-receive-pack";

/// The remote ref map + `.have` hints + capability flags parsed from a
/// receive-pack advertisement. Shared by the duplex ([`push_remote`]) and
/// stateless-HTTP ([`push_http`]) paths so the decision/command/pack logic is
/// identical regardless of how the advertisement was obtained.
struct AdvertisedState {
    /// Real remote refs (name -> oid), excluding the `.have` carrier lines.
    remote_refs: HashMap<String, ObjectId>,
    /// `.have` object hints (objects the remote holds but does not name a ref for).
    advertised_haves: Vec<ObjectId>,
    /// Whether the server advertised `side-band-64k`/`side-band` (report demuxing).
    server_sideband: bool,
    /// Whether the server advertised `ofs-delta` (offset-relative delta bases).
    server_ofs_delta: bool,
    /// Whether the server advertised `push-options` (server-side push options).
    server_push_options: bool,
}

impl AdvertisedState {
    /// Build from a live [`Connection`]'s parsed advertisement. The `.have` lines
    /// are recorded by the connection as refs literally named `.have`, so peel
    /// those out here; everything else is a real remote ref.
    fn from_connection(conn: &mut dyn Connection) -> Self {
        let mut remote_refs: HashMap<String, ObjectId> = HashMap::new();
        let mut advertised_haves: Vec<ObjectId> = Vec::new();
        for (name, oid) in conn.advertised_refs() {
            if name == ".have" {
                advertised_haves.push(*oid);
            } else {
                remote_refs.insert(name.clone(), *oid);
            }
        }
        let caps = conn.capabilities();
        Self {
            remote_refs,
            advertised_haves,
            server_sideband: caps
                .iter()
                .any(|c| c == "side-band-64k" || c == "side-band"),
            server_ofs_delta: caps.iter().any(|c| c == "ofs-delta"),
            server_push_options: caps.iter().any(|c| c == "push-options"),
        }
    }
}

/// A parsed smart-HTTP receive-pack advertisement: protocol version + the shared
/// [`AdvertisedState`].
struct ReceivePackAdvertisement {
    protocol_version: u8,
    state: AdvertisedState,
}

/// Discover the `git-receive-pack` advertisement for `repo_url` over an
/// [`crate::transport::http::HttpClient`] (`GET info/refs?service=git-receive-pack`).
///
/// Lifted from the CLI's `http_push_smart.rs` (`discover_receive_pack` /
/// `read_receive_pack_advertisement`): strips the `# service=…` smart preamble,
/// detects a v2 capability block, and otherwise parses the v0/v1 ref lines
/// (capabilities ride the NUL suffix of the first ref line; `.have` lines and the
/// all-zero capabilities carrier are handled). Hash-width aware via
/// [`ObjectId::from_hex`].
fn discover_receive_pack(
    client: &dyn crate::transport::http::HttpClient,
    repo_url: &str,
) -> Result<ReceivePackAdvertisement> {
    let base = repo_url.trim_end_matches('/');
    let mut refs_url = format!("{base}/info/refs");
    refs_url.push_str(if refs_url.contains('?') { "&" } else { "?" });
    refs_url.push_str("service=");
    refs_url.push_str(RECEIVE_PACK);

    let body = client.get(&refs_url, None)?;
    let pkt_body = strip_service_advertisement(&body)?;
    parse_receive_pack_advertisement(pkt_body)
}

/// The `git-receive-pack` stateless-RPC endpoint URL for `repo_url`.
fn receive_pack_url(repo_url: &str) -> String {
    let base = repo_url.trim_end_matches('/');
    format!("{base}/{RECEIVE_PACK}")
}

/// Strip the optional `# service=git-receive-pack\n` pkt-line + flush preamble a
/// smart-HTTP `info/refs?service=…` response begins with, returning the remaining
/// advertisement bytes. A dumb server (or raw advertisement) omits it.
fn strip_service_advertisement(body: &[u8]) -> Result<&[u8]> {
    let mut cur = Cursor::new(body);
    match pkt_line::read_packet(&mut cur)? {
        Some(Packet::Data(line)) if line.starts_with("# service=") => {
            match pkt_line::read_packet(&mut cur)? {
                Some(Packet::Flush) | None => {}
                _ => return Ok(body),
            }
            let pos = cur.position() as usize;
            Ok(&body[pos..])
        }
        _ => Ok(body),
    }
}

/// Parse a receive-pack advertisement (after the service preamble is stripped)
/// into a [`ReceivePackAdvertisement`].
fn parse_receive_pack_advertisement(body: &[u8]) -> Result<ReceivePackAdvertisement> {
    let mut cur = Cursor::new(body);

    // Peek the first packet to distinguish a v2 capability block from v0/v1.
    let first = match pkt_line::read_packet(&mut cur)? {
        None | Some(Packet::Flush) => {
            return Ok(ReceivePackAdvertisement {
                protocol_version: 0,
                state: AdvertisedState {
                    remote_refs: HashMap::new(),
                    advertised_haves: Vec::new(),
                    server_sideband: false,
                    server_ofs_delta: false,
                    server_push_options: false,
                },
            });
        }
        Some(Packet::Data(s)) => s,
        Some(other) => {
            return Err(Error::Message(format!(
                "unexpected first receive-pack advertisement packet: {other:?}"
            )))
        }
    };
    if first.trim_end() == "version 2" {
        // A v2 advertisement carries no refs/`.have`s here; capabilities live in
        // the following lines. We only need the version (push is v0/v1).
        let mut caps: HashSet<String> = HashSet::new();
        loop {
            match pkt_line::read_packet(&mut cur)? {
                None | Some(Packet::Flush) => break,
                Some(Packet::Data(s)) => {
                    caps.insert(s.trim_end().to_owned());
                }
                Some(_) => break,
            }
        }
        return Ok(ReceivePackAdvertisement {
            protocol_version: 2,
            state: AdvertisedState {
                remote_refs: HashMap::new(),
                advertised_haves: Vec::new(),
                server_sideband: caps
                    .iter()
                    .any(|c| c == "side-band-64k" || c == "side-band"),
                server_ofs_delta: caps.iter().any(|c| c == "ofs-delta"),
                server_push_options: caps.iter().any(|c| c == "push-options"),
            },
        });
    }

    // v0/v1: rewind and parse the ref lines + `.have`s.
    cur.set_position(0);
    let mut remote_refs: HashMap<String, ObjectId> = HashMap::new();
    let mut advertised_haves: Vec<ObjectId> = Vec::new();
    let mut caps: HashSet<String> = HashSet::new();
    let mut first_ref_line = true;
    let mut protocol_version = 0u8;
    loop {
        match pkt_line::read_packet(&mut cur)? {
            None | Some(Packet::Flush) => break,
            Some(Packet::Data(line)) => {
                let line = line.trim_end_matches('\n');
                if line == "version 1" {
                    protocol_version = 1;
                    continue;
                }
                if line.starts_with("version ") || line.starts_with("shallow ") {
                    continue;
                }
                let (payload, cap_part) = match line.split_once('\0') {
                    Some((p, c)) => (p.trim(), Some(c)),
                    None => (line.trim(), None),
                };
                let Some((oid_hex, refname)) =
                    payload.split_once('\t').or_else(|| payload.split_once(' '))
                else {
                    continue;
                };
                let oid_hex = oid_hex.trim();
                let refname = refname.trim();
                if first_ref_line {
                    if let Some(raw_caps) = cap_part {
                        for cap in raw_caps.split_whitespace() {
                            caps.insert(cap.to_owned());
                        }
                    }
                    first_ref_line = false;
                }
                if refname.is_empty() {
                    continue;
                }
                // All-zero OID marks the capabilities-only carrier (empty repo).
                if oid_hex.bytes().all(|b| b == b'0') {
                    continue;
                }
                let oid = ObjectId::from_hex(oid_hex).map_err(|e| {
                    Error::Message(format!("bad oid in receive-pack advertisement: {oid_hex}: {e}"))
                })?;
                if refname == ".have" {
                    advertised_haves.push(oid);
                } else {
                    remote_refs.insert(refname.to_owned(), oid);
                }
            }
            Some(other) => {
                return Err(Error::Message(format!(
                    "unexpected packet in receive-pack advertisement: {other:?}"
                )))
            }
        }
    }
    Ok(ReceivePackAdvertisement {
        protocol_version,
        state: AdvertisedState {
            remote_refs,
            advertised_haves,
            server_sideband: caps
                .iter()
                .any(|c| c == "side-band-64k" || c == "side-band"),
            server_ofs_delta: caps.iter().any(|c| c == "ofs-delta"),
            server_push_options: caps.iter().any(|c| c == "push-options"),
        },
    })
}

/// The accepted, value-changing updates a push will actually send, plus the full
/// per-ref decision list (so client-rejected/up-to-date refs are still reported).
struct PushPlan {
    decisions: Vec<PushDecision>,
    /// Indices into `decisions` of the updates to send a command for.
    to_send: Vec<usize>,
}

/// Outcome of [`plan_push`]: either a [`PushPlan`] to send over the wire, or a
/// terminal set of results (atomic abort, all up-to-date / client-rejected,
/// dry-run) that needs no wire round.
enum PlanOutcome {
    Send(PushPlan),
    Done(Vec<PushRefResult>),
}

/// Decide every [`PushRefSpec`] client-side against the advertised remote refs,
/// applying the atomic / dry-run / nothing-to-send gates. Shared by
/// [`push_remote`] and [`push_http`] so both paths reach the wire with an
/// identical decision set.
fn plan_push(
    refs: &[PushRefSpec],
    local_odb: &crate::odb::Odb,
    local_git_dir: &Path,
    adv: &AdvertisedState,
    opts: &PushOptions,
) -> Result<PlanOutcome> {
    let local_repo = crate::repo::Repository::open(local_git_dir, None).ok();

    let mut decisions: Vec<PushDecision> = Vec::with_capacity(refs.len());
    for spec in refs {
        decisions.push(decide_push_wire(
            spec,
            local_odb,
            &adv.remote_refs,
            local_repo.as_ref(),
        )?);
    }

    // Atomic: a single client-side rejection aborts the whole push without
    // sending anything; the otherwise-accepted updates become AtomicPushFailed.
    let any_rejected = decisions.iter().any(|d| d.result.status.is_error());
    if opts.atomic && any_rejected {
        for d in &mut decisions {
            if matches!(d.result.status, PushRefStatus::Ok) {
                d.result.status = PushRefStatus::AtomicPushFailed;
                d.send = false;
            }
        }
        return Ok(PlanOutcome::Done(
            decisions.into_iter().map(|d| d.result).collect(),
        ));
    }

    let to_send: Vec<usize> = decisions
        .iter()
        .enumerate()
        .filter_map(|(i, d)| if d.send { Some(i) } else { None })
        .collect();

    // Nothing to send (all up-to-date / client-rejected): no wire round needed.
    if to_send.is_empty() || opts.dry_run {
        return Ok(PlanOutcome::Done(
            decisions.into_iter().map(|d| d.result).collect(),
        ));
    }

    Ok(PlanOutcome::Send(PushPlan { decisions, to_send }))
}

/// Reject a push that carries `push_options` when the server's receive-pack did
/// not advertise the `push-options` capability.
///
/// Returns [`Error::PushOptionsUnsupported`] (matching Git's
/// `fatal: the receiving end does not support push options`) so embedders can
/// distinguish this negotiation failure without string-matching. A no-op when
/// `push_options` is empty or the server advertised the capability.
fn require_push_options_supported(adv: &AdvertisedState, opts: &PushOptions) -> Result<()> {
    if !opts.push_options.is_empty() && !adv.server_push_options {
        return Err(Error::PushOptionsUnsupported);
    }
    Ok(())
}

/// Build the ref-update command block: one pkt-line per accepted update plus a
/// trailing flush. The first command carries the negotiated capability list
/// after a NUL; the rest are bare. The OID width is the repository's hash
/// algorithm (zero/null OID for create/delete). Shared by both push paths.
///
/// When `push_options` is non-empty, the negotiated capability list includes
/// `push-options` and, after the command-list flush, one `push-option <value>`
/// pkt-line per option is written followed by a second flush (matching Git's
/// `send-pack`: command-list, flush, push-option lines, flush, then pack). The
/// caller must have already verified the server advertised `push-options`.
fn build_command_block(
    plan: &PushPlan,
    adv: &AdvertisedState,
    algo: HashAlgo,
    push_options: &[String],
) -> Result<Vec<u8>> {
    let zero_hex = "0".repeat(algo.hex_len());
    let mut command_caps = PUSH_CAPS_BASE.to_owned();
    if adv.server_sideband {
        command_caps.push_str(" side-band-64k");
    }
    if !push_options.is_empty() {
        command_caps.push_str(" push-options");
    }
    command_caps.push_str(&format!(" object-format={}", algo.name()));

    let mut commands: Vec<u8> = Vec::new();
    let mut first = true;
    for &i in &plan.to_send {
        let d = &plan.decisions[i];
        let old_hex = d
            .result
            .old_oid
            .map(|o| o.to_hex())
            .unwrap_or_else(|| zero_hex.clone());
        let new_hex = d
            .result
            .new_oid
            .map(|o| o.to_hex())
            .unwrap_or_else(|| zero_hex.clone());
        // `write_line_to_vec` appends the pkt-line's trailing newline itself, so
        // the command payload must NOT carry one of its own; otherwise the bare
        // (second and later) command lines would frame as `<old> <new> <ref>\n`
        // and `git-receive-pack` would read a refname with an embedded newline
        // ("funny refname"). The first line's capability list rides the NUL.
        let line = if first {
            first = false;
            format!("{old_hex} {new_hex} {}\0{command_caps}", d.result.remote_ref)
        } else {
            format!("{old_hex} {new_hex} {}", d.result.remote_ref)
        };
        pkt_line::write_line_to_vec(&mut commands, &line)?;
    }
    // Flush terminates the command list. When push-options are negotiated, the
    // option lines follow this flush and are themselves terminated by a second
    // flush before the pack (per the receive-pack protocol).
    commands.extend_from_slice(b"0000");
    if !push_options.is_empty() {
        for opt in push_options {
            pkt_line::write_line_to_vec(&mut commands, opt)?;
        }
        commands.extend_from_slice(b"0000");
    }
    Ok(commands)
}

/// Build the packfile bytes for a push: a thin, delta-compressed pack of the new
/// tips minus everything the remote already advertised (its ref tips + `.have`s).
///
/// Returns `None` when there is nothing to pack — i.e. a deletion-only push.
/// Matching `git send-pack` (`send-pack.c`: the pack is written only when
/// `need_pack_data && cmds_sent`, and `need_pack_data` is set solely for
/// non-delete updates), no packfile — not even an empty one — is streamed for a
/// pure deletion. `git-receive-pack` does not read a pack after a delete-only
/// command block, so sending one leaves unread bytes on the wire and trips a
/// `ConnectionReset` on the streaming (daemon/ssh) transports. Shared by both
/// push paths so the wire bytes are identical regardless of transport.
fn build_push_pack(
    plan: &PushPlan,
    local_odb: &crate::odb::Odb,
    adv: &AdvertisedState,
) -> Result<Option<Vec<u8>>> {
    let wants: Vec<ObjectId> = plan
        .to_send
        .iter()
        .filter_map(|&i| plan.decisions[i].new_tip)
        .collect();

    if wants.is_empty() {
        return Ok(None);
    }

    let mut haves: Vec<ObjectId> = adv.remote_refs.values().copied().collect();
    haves.extend_from_slice(&adv.advertised_haves);
    // Send a thin, delta-compressed pack: the haves are everything the remote
    // already advertised, so blob deltas may reference those peer-held bases
    // without re-sending them (thin), and OFS_DELTA is used only when the server
    // advertised the `ofs-delta` capability.
    build_pack(
        local_odb,
        &wants,
        &haves,
        &PackBuildOptions {
            thin: true,
            delta: true,
            use_ofs_delta: adv.server_ofs_delta,
            ..PackBuildOptions::default()
        },
    )
    .map(Some)
}

/// A client-side push decision for one ref, plus what to send over the wire.
struct PushDecision {
    result: PushRefResult,
    /// The new tip object to pack (None for deletions / no-ops).
    new_tip: Option<ObjectId>,
    /// Whether to send a ref-update command for this ref to the server.
    send: bool,
}

/// Decide one [`PushRefSpec`] against the advertised remote refs, without any
/// I/O to the remote. Mirrors [`crate::transfer`]'s `decide_push`, but the
/// "remote current" value comes from the advertisement map rather than an
/// on-disk remote ref.
fn decide_push_wire(
    spec: &PushRefSpec,
    local_odb: &crate::odb::Odb,
    remote_refs: &HashMap<String, ObjectId>,
    local_repo: Option<&crate::repo::Repository>,
) -> Result<PushDecision> {
    let remote_current = remote_refs.get(&spec.dst).copied();

    let no_op = |status: PushRefStatus,
                 old: Option<ObjectId>,
                 new: Option<ObjectId>,
                 deletion: bool,
                 message: Option<String>| {
        PushDecision {
            result: PushRefResult {
                local_ref: None,
                remote_ref: spec.dst.clone(),
                old_oid: old,
                new_oid: new,
                forced: false,
                deletion,
                status,
                message,
            },
            new_tip: None,
            send: false,
        }
    };

    // Up-to-date trumps every lease (creating/moving a ref to where it already
    // is succeeds, even when a force-with-lease expectation does not hold).
    if !spec.delete {
        if let Some(src) = spec.src {
            if remote_current == Some(src) {
                return Ok(no_op(
                    PushRefStatus::UpToDate,
                    remote_current,
                    Some(src),
                    false,
                    None,
                ));
            }
        }
    }

    // Absence lease: a destination that already exists fails the lease.
    if spec.expect_absent && remote_current.is_some() {
        return Ok(no_op(
            PushRefStatus::RejectStale,
            remote_current,
            spec.src,
            spec.delete,
            Some("stale info".to_owned()),
        ));
    }

    // Compare-and-swap (force-with-lease): the remote's current value must match.
    if let Some(expected) = spec.expected_old {
        if remote_current != Some(expected) {
            return Ok(no_op(
                PushRefStatus::RejectStale,
                remote_current,
                spec.src,
                spec.delete,
                Some("stale info".to_owned()),
            ));
        }
    }

    if spec.delete {
        // Deleting a ref the remote does not have is a no-op success; otherwise
        // send the delete command (null new OID) and let the server confirm.
        return Ok(match remote_current {
            Some(_) => PushDecision {
                result: PushRefResult {
                    local_ref: None,
                    remote_ref: spec.dst.clone(),
                    old_oid: remote_current,
                    new_oid: None,
                    forced: false,
                    deletion: true,
                    status: PushRefStatus::Ok,
                    message: None,
                },
                new_tip: None,
                send: true,
            },
            None => no_op(PushRefStatus::UpToDate, None, None, true, None),
        });
    }

    let Some(src) = spec.src else {
        return Err(Error::Message(format!(
            "push to '{}' has no source object and is not a deletion",
            spec.dst
        )));
    };
    if !local_odb.exists(&src) {
        return Err(Error::Message(format!(
            "source object {src} for '{}' is missing from the local object store",
            spec.dst
        )));
    }

    // New ref: nothing on the remote yet — always allowed.
    let Some(old) = remote_current else {
        return Ok(PushDecision {
            result: PushRefResult {
                local_ref: None,
                remote_ref: spec.dst.clone(),
                old_oid: None,
                new_oid: Some(src),
                forced: false,
                deletion: false,
                status: PushRefStatus::Ok,
                message: None,
            },
            new_tip: Some(src),
            send: true,
        });
    };

    // Existing ref: fast-forward when the remote's current commit is an ancestor
    // of the source; otherwise non-fast-forward (allowed only with force).
    let is_ff = local_repo
        .map(|r| crate::merge_base::is_ancestor(r, old, src).unwrap_or(false))
        .unwrap_or(false);

    if is_ff {
        Ok(PushDecision {
            result: PushRefResult {
                local_ref: None,
                remote_ref: spec.dst.clone(),
                old_oid: Some(old),
                new_oid: Some(src),
                forced: false,
                deletion: false,
                status: PushRefStatus::Ok,
                message: None,
            },
            new_tip: Some(src),
            send: true,
        })
    } else if spec.force {
        Ok(PushDecision {
            result: PushRefResult {
                local_ref: None,
                remote_ref: spec.dst.clone(),
                old_oid: Some(old),
                new_oid: Some(src),
                forced: true,
                deletion: false,
                status: PushRefStatus::Ok,
                message: None,
            },
            new_tip: Some(src),
            send: true,
        })
    } else {
        Ok(PushDecision {
            result: PushRefResult {
                local_ref: None,
                remote_ref: spec.dst.clone(),
                old_oid: Some(old),
                new_oid: Some(src),
                forced: false,
                deletion: false,
                status: PushRefStatus::RejectNonFastForward,
                message: Some("non-fast-forward".to_owned()),
            },
            new_tip: None,
            send: false,
        })
    }
}

/// Parse the server's `report-status` / `report-status-v2` stream and fold each
/// per-ref `ok`/`ng` line back into the matching decision.
///
/// The report is:
/// ```text
/// unpack ok\n            (or `unpack <error>\n`)
/// ok <ref>\n             (per accepted ref)
/// ng <ref> <reason>\n    (per rejected ref)
/// ```
/// An `ng` line demotes the decided result to [`PushRefStatus::RemoteRejected`]
/// with the server's reason; an `unpack` failure demotes every sent ref. Lifted
/// from the CLI's `report_has_rejections`, extended to capture the reason and the
/// `unpack` status.
fn apply_report_status(report: &[u8], decisions: &mut [PushDecision]) {
    let mut by_ref: HashMap<&str, usize> = HashMap::new();
    for (i, d) in decisions.iter().enumerate() {
        if d.send {
            by_ref.insert(d.result.remote_ref.as_str(), i);
        }
    }
    // Resolve indices up front to avoid borrow conflicts while mutating.
    let mut unpack_error: Option<String> = None;
    let mut updates: Vec<(usize, Option<String>)> = Vec::new();

    let mut cursor = Cursor::new(report);
    while let Ok(Some(pkt)) = pkt_line::read_packet(&mut cursor) {
        let Packet::Data(line) = pkt else {
            continue;
        };
        let line = line.trim_end();
        if let Some(rest) = line.strip_prefix("unpack ") {
            if rest.trim() != "ok" {
                unpack_error = Some(rest.trim().to_owned());
            }
        } else if let Some(refname) = line.strip_prefix("ok ") {
            // Accepted: keep the decided (Ok/UpToDate) status.
            let _ = by_ref.get(refname.trim());
        } else if let Some(rest) = line.strip_prefix("ng ") {
            // `ng <ref> <reason>`: the remote declined this update.
            let (refname, reason) = rest.split_once(' ').unwrap_or((rest, ""));
            if let Some(&idx) = by_ref.get(refname.trim()) {
                let msg = if reason.trim().is_empty() {
                    None
                } else {
                    Some(reason.trim().to_owned())
                };
                updates.push((idx, msg));
            }
        }
    }

    for (idx, msg) in updates {
        decisions[idx].result.status = PushRefStatus::RemoteRejected;
        decisions[idx].result.message = msg;
    }

    // A failed `unpack` rejects every ref we sent that the server did not
    // already mark as failed.
    if let Some(reason) = unpack_error {
        for d in decisions.iter_mut() {
            if d.send && !matches!(d.result.status, PushRefStatus::RemoteRejected) {
                d.result.status = PushRefStatus::RemoteRejected;
                d.result.message = Some(format!("unpack failed: {reason}"));
            }
        }
    }
}

/// Split a side-band stream: band 1 (report-status) is returned; band 2/3
/// (remote diagnostics) is forwarded to `progress`. Lifted from the CLI's
/// `demux_report_and_remote_messages`, but progress goes to the callback rather
/// than directly to stderr (the public API must not assume stdout/stderr).
fn demux_report_and_remote_messages(
    input: &[u8],
    progress: &mut dyn Progress,
) -> Result<Vec<u8>> {
    let mut report = Vec::new();
    let mut i = 0usize;
    while i + 4 <= input.len() {
        let len = match pkt_line::parse_hex_len(&input[i..i + 4]) {
            Ok(l) => l,
            Err(_) => break,
        };
        i += 4;
        if len == 0 {
            // Flush packet: a delimiter between report sections, keep scanning.
            continue;
        }
        if len < 4 || i + (len - 4) > input.len() {
            break;
        }
        let payload = &input[i..i + (len - 4)];
        i += len - 4;
        if payload.is_empty() {
            continue;
        }
        let band = payload[0];
        let data = &payload[1..];
        match band {
            1 => report.extend_from_slice(data),
            2 | 3 => progress.message(data),
            _ => {}
        }
    }
    Ok(report)
}

/// Peel `oid` to the commit it ultimately names, following annotated tags, using
/// the local odb. Returns `None` if it is not a commit (or is missing). Provided
/// for symmetry with the CLI's `peel_advertised_commits`; the wire `build_pack`
/// uses the advertised ref/`.have` oids directly as `haves`, so this is exposed
/// for callers that need commit tips.
#[allow(dead_code)]
fn peel_to_commit(odb: &crate::odb::Odb, oid: ObjectId) -> Option<ObjectId> {
    let mut current = oid;
    for _ in 0..16 {
        let obj = odb.read(&current).ok()?;
        match obj.kind {
            ObjectKind::Commit => return Some(current),
            ObjectKind::Tag => current = parse_tag(&obj.data).ok()?.object,
            _ => return None,
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_decision(refname: &str, send: bool) -> PushDecision {
        PushDecision {
            result: PushRefResult {
                local_ref: None,
                remote_ref: refname.to_owned(),
                old_oid: None,
                new_oid: None,
                forced: false,
                deletion: false,
                status: PushRefStatus::Ok,
                message: None,
            },
            new_tip: None,
            send,
        }
    }

    fn report_bytes(lines: &[&str]) -> Vec<u8> {
        let mut buf = Vec::new();
        for l in lines {
            pkt_line::write_line_to_vec(&mut buf, l).unwrap();
        }
        buf.extend_from_slice(b"0000");
        buf
    }

    fn adv_state(sideband: bool, ofs_delta: bool, push_options: bool) -> AdvertisedState {
        AdvertisedState {
            remote_refs: HashMap::new(),
            advertised_haves: Vec::new(),
            server_sideband: sideband,
            server_ofs_delta: ofs_delta,
            server_push_options: push_options,
        }
    }

    /// Decode a command block into a flat list of packets: each `Data(line)`
    /// becomes `Some(line)` and each flush becomes `None`, so the framing
    /// (command lines / flush / push-option lines / flush) is fully visible.
    fn decode_block(block: &[u8]) -> Vec<Option<String>> {
        let mut cur = Cursor::new(block);
        let mut out = Vec::new();
        while let Ok(pkt) = pkt_line::read_packet(&mut cur) {
            match pkt {
                Some(Packet::Data(s)) => out.push(Some(s.trim_end_matches('\n').to_owned())),
                Some(Packet::Flush) => out.push(None),
                _ => break,
            }
        }
        out
    }

    fn send_decision(refname: &str, new_oid: ObjectId) -> PushDecision {
        PushDecision {
            result: PushRefResult {
                local_ref: None,
                remote_ref: refname.to_owned(),
                old_oid: None,
                new_oid: Some(new_oid),
                forced: false,
                deletion: false,
                status: PushRefStatus::Ok,
                message: None,
            },
            new_tip: Some(new_oid),
            send: true,
        }
    }

    #[test]
    fn command_block_without_push_options_has_no_capability_or_lines() {
        let new = ObjectId::from_hex(&"1".repeat(40)).unwrap();
        let plan = PushPlan {
            decisions: vec![send_decision("refs/heads/main", new)],
            to_send: vec![0],
        };
        let block =
            build_command_block(&plan, &adv_state(false, false, true), HashAlgo::Sha1, &[]).unwrap();
        let pkts = decode_block(&block);
        // command line, then a single terminating flush — nothing else.
        assert_eq!(pkts.len(), 2);
        let cmd = pkts[0].as_deref().unwrap();
        assert!(
            cmd.contains("refs/heads/main"),
            "first line is the ref command, got {cmd:?}"
        );
        assert!(
            !cmd.contains("push-options"),
            "no push-options capability without options, got {cmd:?}"
        );
        assert_eq!(pkts[1], None, "single trailing flush");
    }

    #[test]
    fn command_block_with_push_options_negotiates_cap_and_emits_lines() {
        let new = ObjectId::from_hex(&"1".repeat(40)).unwrap();
        let plan = PushPlan {
            decisions: vec![send_decision("refs/heads/main", new)],
            to_send: vec![0],
        };
        let opts = vec!["ci.skip".to_owned(), "reviewer=alice".to_owned()];
        let block = build_command_block(
            &plan,
            &adv_state(true, true, true),
            HashAlgo::Sha1,
            &opts,
        )
        .unwrap();
        let pkts = decode_block(&block);
        // command line | flush | push-option ci.skip | push-option reviewer=alice | flush
        assert_eq!(
            pkts,
            vec![
                pkts[0].clone(),
                None,
                Some("ci.skip".to_owned()),
                Some("reviewer=alice".to_owned()),
                None,
            ],
            "push-option lines must follow the command-list flush, then a flush"
        );
        let cmd = pkts[0].as_deref().unwrap();
        assert!(
            cmd.contains("push-options"),
            "capability list must advertise push-options, got {cmd:?}"
        );
        // The first command line still carries the rest of the negotiated caps.
        assert!(cmd.contains("report-status"));
        assert!(cmd.contains("side-band-64k"));
        assert!(cmd.contains("object-format=sha1"));
    }

    #[test]
    fn require_push_options_errors_typed_when_server_lacks_capability() {
        let opts = PushOptions {
            push_options: vec!["x".to_owned()],
            ..PushOptions::default()
        };
        // Server did NOT advertise push-options: typed error, not Message.
        let err = require_push_options_supported(&adv_state(true, true, false), &opts).unwrap_err();
        assert!(
            matches!(err, Error::PushOptionsUnsupported),
            "expected PushOptionsUnsupported, got {err:?}"
        );
        assert_eq!(
            err.to_string(),
            "the receiving end does not support push options"
        );
        // Server advertised it: ok.
        require_push_options_supported(&adv_state(true, true, true), &opts).unwrap();
        // No options: ok regardless of capability.
        require_push_options_supported(&adv_state(true, true, false), &PushOptions::default())
            .unwrap();
    }

    #[test]
    fn receive_pack_url_and_strip_preamble() {
        assert_eq!(
            receive_pack_url("http://h/r.git/"),
            "http://h/r.git/git-receive-pack"
        );
        // The `# service=…` smart preamble + flush is stripped; the ref bytes remain.
        let mut tail = Vec::new();
        pkt_line::write_line_to_vec(&mut tail, &format!("{} refs/heads/main", "1".repeat(40)))
            .unwrap();
        tail.extend_from_slice(b"0000");

        let mut body = Vec::new();
        pkt_line::write_line_to_vec(&mut body, "# service=git-receive-pack\n").unwrap();
        body.extend_from_slice(b"0000");
        body.extend_from_slice(&tail);
        assert_eq!(strip_service_advertisement(&body).unwrap(), tail.as_slice());
        // A body without the preamble is returned verbatim.
        assert_eq!(strip_service_advertisement(&tail).unwrap(), tail.as_slice());
    }

    #[test]
    fn parses_v0_receive_pack_advertisement_with_caps_and_have() {
        let main = "1".repeat(40);
        let have = "2".repeat(40);
        let mut body = Vec::new();
        // First ref line carries the receive-pack capabilities after a NUL.
        pkt_line::write_line_to_vec(
            &mut body,
            &format!(
                "{main} refs/heads/main\0report-status report-status-v2 side-band-64k ofs-delta object-format=sha1"
            ),
        )
        .unwrap();
        // A `.have` hint line (object the remote holds, not named by a ref).
        pkt_line::write_line_to_vec(&mut body, &format!("{have} .have")).unwrap();
        body.extend_from_slice(b"0000");

        let adv = parse_receive_pack_advertisement(&body).unwrap();
        assert_eq!(adv.protocol_version, 0);
        assert!(adv.state.server_sideband);
        assert!(adv.state.server_ofs_delta);
        assert_eq!(
            adv.state.remote_refs.get("refs/heads/main").map(|o| o.to_hex()),
            Some(main.clone())
        );
        assert_eq!(adv.state.advertised_haves.len(), 1);
        assert_eq!(adv.state.advertised_haves[0].to_hex(), have);
        // The `.have` carrier is not exposed as a real ref.
        assert!(!adv.state.remote_refs.contains_key(".have"));
    }

    #[test]
    fn parses_empty_repo_capabilities_carrier() {
        // An empty receive-pack target advertises a single all-zero capabilities
        // carrier line; it contributes no refs but still yields the caps.
        let zero = "0".repeat(40);
        let mut body = Vec::new();
        pkt_line::write_line_to_vec(
            &mut body,
            &format!("{zero} capabilities^{{}}\0report-status delete-refs ofs-delta"),
        )
        .unwrap();
        body.extend_from_slice(b"0000");

        let adv = parse_receive_pack_advertisement(&body).unwrap();
        assert_eq!(adv.protocol_version, 0);
        assert!(adv.state.remote_refs.is_empty());
        assert!(adv.state.advertised_haves.is_empty());
        assert!(adv.state.server_ofs_delta);
        assert!(!adv.state.server_sideband);
    }

    #[test]
    fn detects_v2_receive_pack_advertisement() {
        let mut body = Vec::new();
        pkt_line::write_line_to_vec(&mut body, "version 2").unwrap();
        pkt_line::write_line_to_vec(&mut body, "agent=grit/test").unwrap();
        pkt_line::write_line_to_vec(&mut body, "object-format=sha1").unwrap();
        body.extend_from_slice(b"0000");
        let adv = parse_receive_pack_advertisement(&body).unwrap();
        assert_eq!(adv.protocol_version, 2);
    }

    #[test]
    fn report_ng_demotes_to_remote_rejected() {
        let mut decisions = vec![
            make_decision("refs/heads/main", true),
            make_decision("refs/heads/topic", true),
        ];
        let report = report_bytes(&[
            "unpack ok",
            "ok refs/heads/main",
            "ng refs/heads/topic non-fast-forward",
        ]);
        apply_report_status(&report, &mut decisions);
        assert_eq!(decisions[0].result.status, PushRefStatus::Ok);
        assert_eq!(decisions[1].result.status, PushRefStatus::RemoteRejected);
        assert_eq!(
            decisions[1].result.message.as_deref(),
            Some("non-fast-forward")
        );
    }

    #[test]
    fn report_unpack_failure_rejects_all_sent() {
        let mut decisions = vec![make_decision("refs/heads/main", true)];
        let report = report_bytes(&["unpack index-pack abort"]);
        apply_report_status(&report, &mut decisions);
        assert_eq!(decisions[0].result.status, PushRefStatus::RemoteRejected);
        assert!(decisions[0]
            .result
            .message
            .as_deref()
            .unwrap()
            .starts_with("unpack failed:"));
    }

    #[test]
    fn demux_separates_report_and_progress() {
        struct Cap(Vec<u8>);
        impl Progress for Cap {
            fn message(&mut self, bytes: &[u8]) {
                self.0.extend_from_slice(bytes);
            }
        }
        // Band 1 = report, band 2 = progress.
        let mut wire = Vec::new();
        let mut band1 = vec![1u8];
        band1.extend_from_slice(b"unpack ok\n");
        pkt_line::write_packet_raw(&mut wire, &band1).unwrap();
        let mut band2 = vec![2u8];
        band2.extend_from_slice(b"hello from hook\n");
        pkt_line::write_packet_raw(&mut wire, &band2).unwrap();
        wire.extend_from_slice(b"0000");

        let mut cap = Cap(Vec::new());
        let report = demux_report_and_remote_messages(&wire, &mut cap).unwrap();
        assert_eq!(report, b"unpack ok\n");
        assert_eq!(cap.0, b"hello from hook\n");
    }
}
