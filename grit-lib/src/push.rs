//! Wire-protocol push orchestration over a [`crate::transport::Connection`].
//!
//! [`push_remote`] is the wire counterpart to [`crate::transfer::push_local`]:
//! instead of copying objects between two on-disk repositories, it drives a
//! `git-receive-pack` exchange over a live [`crate::transport::Connection`] —
//! reading the receive-pack advertisement (remote refs + `.have` lines +
//! capabilities), deciding each ref update against the advertised remote refs
//! (reusing the same fast-forward / force / force-with-lease rules as
//! `push_local`), building the minimal pack with [`crate::pack_objects::build_pack_for_push`]
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

use std::collections::{HashMap, HashSet};
use std::io::{Cursor, ErrorKind, Read};
use std::path::Path;

use crate::branch_tracking;
use crate::environment::RepositoryOptions;
use crate::error::{Error, Result};
use crate::fetch::Progress;
use crate::objects::{HashAlgo, ObjectId};
use crate::pkt_line::{self, Packet};
use crate::progress::{poll_cancel, Cancel, ProgressPhase, ProgressUnit, ReceivedObjects};
use crate::push_report::{PushRefResult, PushRefStatus};
use crate::repo::Repository;
use crate::sideband_lines::{RemoteLine, SidebandLines};
use crate::transfer::{PackBuildOptions, PushOptions, PushOutcome, PushRefSpec};
use crate::transport::Connection;

fn finish_push_outcome(
    local_git_dir: &Path,
    opts: &PushOptions,
    results: Vec<PushRefResult>,
    remote_messages: Vec<String>,
) -> Result<PushOutcome> {
    if !opts.dry_run {
        if let Some(remote) = opts.tracking_remote.as_deref() {
            branch_tracking::apply_push_remote_tracking_updates(local_git_dir, remote, &results)?;
        }
    }
    Ok(PushOutcome {
        results,
        remote_messages,
    })
}

/// Forwards side-band channel 2 to an inner [`Progress`] sink and records complete lines.
struct PushSidebandCollector<'a> {
    inner: &'a mut dyn Progress,
    remote_messages: Vec<String>,
    pending: SidebandLines,
}

impl PushSidebandCollector<'_> {
    fn record_remote_chunk(&mut self, bytes: &[u8]) {
        let mut lines = Vec::new();
        self.pending.feed(bytes, |line| lines.push(line));
        for line in lines {
            self.record_remote_line(line);
        }
    }

    fn record_remote_line(&mut self, line: RemoteLine) {
        if let RemoteLine::Line(text) = line {
            if !text.is_empty() {
                self.remote_messages.push(text);
            }
        }
    }

    fn finish_pending(&mut self) {
        let mut lines = Vec::new();
        self.pending.finish(|line| lines.push(line));
        for line in lines {
            self.record_remote_line(line);
        }
    }
}

impl crate::progress::ProgressSink for PushSidebandCollector<'_> {
    fn start(&mut self, phase: ProgressPhase, total: Option<u64>) {
        self.inner.start(phase, total);
    }

    fn start_with_unit(&mut self, phase: ProgressPhase, unit: ProgressUnit, total: Option<u64>) {
        self.inner.start_with_unit(phase, unit, total);
    }

    fn inc(&mut self, units: u64) {
        self.inner.inc(units);
    }

    fn set(&mut self, current: u64) {
        self.inner.set(current);
    }

    fn finish(&mut self) {
        self.inner.finish();
    }

    fn should_cancel(&self) -> bool {
        self.inner.should_cancel()
    }
}

impl Progress for PushSidebandCollector<'_> {
    fn message(&mut self, bytes: &[u8]) {
        self.inner.message(bytes);
        self.record_remote_chunk(bytes);
    }

    fn remote_sideband_progress(&self) -> bool {
        self.inner.remote_sideband_progress()
    }

    fn wants_received_objects(&self) -> bool {
        self.inner.wants_received_objects()
    }

    fn received_objects(&mut self, counts: ReceivedObjects) {
        self.inner.received_objects(counts);
    }
}

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
///    `ofs-delta`, `object-format`, `atomic`).
/// 2. Decide each [`PushRefSpec`] against the advertised remote refs (up-to-date,
///    new, fast-forward, forced, non-fast-forward rejection, force-with-lease
///    stale) — the client-side gate before anything is sent.
/// 3. Write the ref-update commands for the accepted, value-changing updates
///    (`<old> <new> <ref>\0<caps>\n` first, `<old> <new> <ref>\n` rest), then a
///    flush.
/// 4. Build the minimal pack with [`crate::pack_objects::build_pack_for_push`] (wants = new tips, haves =
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
/// failure. An atomic push to a server without `atomic` support returns
/// [`Error::PushAtomicUnsupported`] before sending updates or objects.
pub fn push_remote(
    local_git_dir: &Path,
    conn: &mut dyn Connection,
    refs: &[PushRefSpec],
    opts: &PushOptions,
    progress: &mut dyn Progress,
    cancel: &dyn Cancel,
) -> Result<PushOutcome> {
    let local = Repository::open(local_git_dir, &RepositoryOptions::empty())?;
    push_remote_from(&local, conn, refs, opts, progress, cancel)
}

/// Like [`push_remote`], reading the objects to send through `local`'s object store.
pub(crate) fn push_remote_from(
    local: &Repository,
    conn: &mut dyn Connection,
    refs: &[PushRefSpec],
    opts: &PushOptions,
    progress: &mut dyn Progress,
    cancel: &dyn Cancel,
) -> Result<PushOutcome> {
    crate::trace::trace_span!("grit.push");
    let local_git_dir = local.git_dir();
    let mut sideband = PushSidebandCollector {
        inner: progress,
        remote_messages: Vec::new(),
        pending: SidebandLines::new(),
    };
    poll_cancel(&mut sideband, cancel)?;
    use crate::net_trace::net_trace;
    net_trace!(
        opts.network_trace,
        opts.diagnostics.as_ref(),
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

    let algo = local.odb().hash_algo();

    // 1. Advertisement: split the connection's parsed advertisement into the
    //    remote ref map and the `.have` hints, and read the negotiated caps.
    let adv = AdvertisedState::from_connection(conn);
    net_trace!(
        opts.network_trace,
        opts.diagnostics.as_ref(),
        "push_remote: remote advertised {} ref(s)",
        adv.remote_refs.len()
    );

    // Required capabilities must be checked before even a no-op flush, so an
    // unsupported atomic push cannot fall back to independent ref updates.
    require_push_capabilities(&adv, opts)?;

    // 2–3. Decide each ref update client-side, handling atomic/dry-run/no-op
    //       early-returns; the shared planner mirrors `push_local`'s gate.
    let mut plan = match plan_push(refs, local, &adv, opts)? {
        PlanOutcome::Send(plan) => plan,
        PlanOutcome::Done(results) => {
            if !opts.dry_run {
                crate::transport::send_client_flush(conn)?;
            }
            return finish_push_outcome(local_git_dir, opts, results, Vec::new());
        }
    };

    // 4. Write the ref-update commands (first carries the cap list after a NUL),
    //    then the pack — but only when there are objects to send. A deletion-only
    //    push streams no pack at all (matching `git send-pack`); `receive-pack`
    //    does not read one after a delete-only command block, so sending an empty
    //    pack would leave unread bytes on the wire and reset the connection.
    let commands = build_command_block(&plan, &adv, algo, opts)?;
    net_trace!(
        opts.network_trace,
        opts.diagnostics.as_ref(),
        "push_remote: sending {} command(s)…",
        plan.decisions.len()
    );
    conn.writer().write_all(&commands)?;
    conn.writer().flush()?;

    if let Some(pack) = build_push_pack(&plan, local, &adv)? {
        poll_cancel(&mut sideband, cancel)?;
        net_trace!(
            opts.network_trace,
            opts.diagnostics.as_ref(),
            "push_remote: sending pack ({} bytes)…",
            pack.len()
        );
        conn.writer().write_all(&pack)?;
        conn.writer().flush()?;
    } else {
        net_trace!(
            opts.network_trace,
            opts.diagnostics.as_ref(),
            "push_remote: no pack (deletion-only / up-to-date)"
        );
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
    // own output once the report is written; the write half is released when the
    // connection is dropped (after the child has already exited, so the `Drop`
    // teardown does not block).
    //
    // Some receive-pack hosts (notably Git-for-Windows `git daemon` on Windows)
    // reset the TCP connection immediately after sending a complete report-status
    // instead of shutting down cleanly. Treat that as success when the bytes we
    // already received parse as a full report for every ref we sent.
    let sent_refs: Vec<&str> = plan
        .to_send
        .iter()
        .map(|&i| plan.decisions[i].result.remote_ref.as_str())
        .collect();
    let raw = read_receive_pack_response(conn.reader(), &sent_refs, adv.server_sideband)?;
    let report = if adv.server_sideband {
        demux_report_and_remote_messages(&raw, &mut sideband)?
    } else {
        raw
    };

    apply_report_status(&report, &mut plan.decisions);

    let results: Vec<_> = plan.decisions.into_iter().map(|d| d.result).collect();
    sideband.finish_pending();
    let remote_messages = sideband.remote_messages;
    net_trace!(
        opts.network_trace,
        opts.diagnostics.as_ref(),
        "push_remote: done — {} result(s)",
        results.len()
    );
    finish_push_outcome(local_git_dir, opts, results, remote_messages)
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
/// wire/parse I/O failure. An atomic push to a server without `atomic` support
/// returns [`Error::PushAtomicUnsupported`] before sending updates or objects.
pub fn push_http(
    client: std::sync::Arc<dyn crate::transport::http::HttpClient>,
    local_git_dir: &Path,
    repo_url: &str,
    refs: &[PushRefSpec],
    opts: &PushOptions,
    progress: &mut dyn Progress,
    cancel: &dyn Cancel,
) -> Result<PushOutcome> {
    use crate::transport::http::SmartHttpTransport;
    use crate::transport::{ConnectOptions, Service, Transport};
    let connect = ConnectOptions {
        protocol_version: 0,
        diagnostics: opts.diagnostics.clone(),
        network_trace: opts.network_trace,
        ..Default::default()
    };
    let transport = SmartHttpTransport::new(client);
    let mut conn = transport.connect(repo_url, Service::ReceivePack, &connect)?;
    push_remote(local_git_dir, &mut *conn, refs, opts, progress, cancel)
}

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
    /// Whether the server advertised `atomic` (one transaction for all refs).
    server_atomic: bool,
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
            server_atomic: caps.iter().any(|c| c == "atomic"),
        }
    }
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
    local: &Repository,
    adv: &AdvertisedState,
    opts: &PushOptions,
) -> Result<PlanOutcome> {
    let mut decisions: Vec<PushDecision> = Vec::with_capacity(refs.len());
    for spec in refs {
        decisions.push(decide_push_wire(spec, local, &adv.remote_refs)?);
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

/// Validate capabilities required by the requested push options before writing
/// any commands or pack data, including for dry-run and no-op pushes.
fn require_push_capabilities(adv: &AdvertisedState, opts: &PushOptions) -> Result<()> {
    if opts.atomic && !adv.server_atomic {
        return Err(Error::PushAtomicUnsupported);
    }
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
/// Atomic updates request `atomic` only when the server supports it. The caller
/// must validate required capabilities with [`require_push_capabilities`] first.
///
/// When `opts.push_options` is non-empty, the negotiated capability list includes
/// `push-options` and, after the command-list flush, one `push-option <value>`
/// pkt-line per option is written followed by a second flush (matching Git's
/// `send-pack`: command-list, flush, push-option lines, flush, then pack). The
/// caller must have already verified the server advertised `push-options`.
fn build_command_block(
    plan: &PushPlan,
    adv: &AdvertisedState,
    algo: HashAlgo,
    opts: &PushOptions,
) -> Result<Vec<u8>> {
    let zero_hex = "0".repeat(algo.hex_len());
    let mut command_caps = PUSH_CAPS_BASE.to_owned();
    if adv.server_sideband {
        command_caps.push_str(" side-band-64k");
    }
    if !opts.push_options.is_empty() {
        command_caps.push_str(" push-options");
    }
    if opts.atomic && adv.server_atomic {
        command_caps.push_str(" atomic");
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
            format!(
                "{old_hex} {new_hex} {}\0{command_caps}",
                d.result.remote_ref
            )
        } else {
            format!("{old_hex} {new_hex} {}", d.result.remote_ref)
        };
        pkt_line::write_line_to_vec(&mut commands, &line)?;
    }
    // Flush terminates the command list. When push-options are negotiated, the
    // option lines follow this flush and are themselves terminated by a second
    // flush before the pack (per the receive-pack protocol).
    commands.extend_from_slice(b"0000");
    if !opts.push_options.is_empty() {
        for opt in &opts.push_options {
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
    local: &Repository,
    adv: &AdvertisedState,
) -> Result<Option<Vec<u8>>> {
    let local_odb = local.odb();
    let wants: Vec<ObjectId> = plan
        .to_send
        .iter()
        .filter_map(|&i| plan.decisions[i].new_tip)
        .collect();

    if wants.is_empty() {
        return Ok(None);
    }

    let haves = local_odb.present_oids(
        adv.remote_refs
            .values()
            .chain(&adv.advertised_haves)
            .copied(),
    )?;
    let local_cfg = crate::config::ConfigSet::load(
        &crate::environment::Environment::empty(),
        Some(local.git_dir()),
        true,
    )
    .ok();
    let mut pack_opts = if haves.is_empty() {
        PackBuildOptions::for_local_copy(local_cfg.as_ref())
    } else {
        PackBuildOptions::for_local_push(local_cfg.as_ref())
    };
    pack_opts.use_ofs_delta = adv.server_ofs_delta;
    crate::pack_objects::build_pack_for_push(local_odb, &wants, &haves, &pack_opts).map(Some)
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
    local: &Repository,
    remote_refs: &HashMap<String, ObjectId>,
) -> Result<PushDecision> {
    let local_odb = local.odb();
    let remote_current = remote_refs.get(&spec.dst).copied();

    let no_op = |status: PushRefStatus,
                 old: Option<ObjectId>,
                 new: Option<ObjectId>,
                 deletion: bool,
                 message: Option<String>| {
        PushDecision {
            result: PushRefResult {
                local_ref: crate::transfer::push_spec_local_ref(spec),
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
        return Ok(match remote_current {
            Some(_) => PushDecision {
                result: PushRefResult {
                    local_ref: crate::transfer::push_spec_local_ref(spec),
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
            None => no_op(
                PushRefStatus::RejectRemoteRefMissing,
                None,
                None,
                true,
                Some("remote ref does not exist".to_owned()),
            ),
        });
    }

    let Some(src) = spec.src else {
        return Err(Error::Message(format!(
            "push to '{}' has no source object and is not a deletion",
            spec.dst
        )));
    };
    if !local_odb.contains(&src)? {
        return Err(Error::Message(format!(
            "source object {src} for '{}' is missing from the local object store",
            spec.dst
        )));
    }

    // New ref: nothing on the remote yet — always allowed.
    let Some(old) = remote_current else {
        return Ok(PushDecision {
            result: PushRefResult {
                local_ref: crate::transfer::push_spec_local_ref(spec),
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

    if !spec.force && !local_odb.contains(&old)? {
        return Ok(no_op(
            PushRefStatus::RejectFetchFirst,
            Some(old),
            Some(src),
            false,
            Some("fetch first".to_owned()),
        ));
    }

    // Existing ref: fast-forward when the remote's current commit is an ancestor
    // of the source; otherwise non-fast-forward (allowed only with force).
    let is_ff = if spec.force {
        false
    } else if crate::merge_base::is_ancestor_odb_walk(local_odb, old, src)? {
        true
    } else {
        crate::merge_base::is_ancestor(local, old, src).unwrap_or(false)
    };

    if is_ff {
        Ok(PushDecision {
            result: PushRefResult {
                local_ref: crate::transfer::push_spec_local_ref(spec),
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
                local_ref: crate::transfer::push_spec_local_ref(spec),
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
                local_ref: crate::transfer::push_spec_local_ref(spec),
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

/// Read the receive-pack report stream until EOF or an abrupt connection close.
///
/// When the server resets the connection after sending a complete
/// `report-status`, the accumulated bytes are returned as success so tracking
/// refs can be updated. Incomplete buffers still surface as I/O errors.
fn read_receive_pack_response(
    reader: &mut dyn Read,
    sent_refs: &[&str],
    server_sideband: bool,
) -> Result<Vec<u8>> {
    let mut raw = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => raw.extend_from_slice(&buf[..n]),
            Err(e) if receive_pack_read_recoverable(&e) => {
                if receive_pack_response_complete(&raw, sent_refs, server_sideband)? {
                    break;
                }
                return Err(Error::Io(e));
            }
            Err(e) => return Err(Error::Io(e)),
        }
    }
    if !receive_pack_response_complete(&raw, sent_refs, server_sideband)? {
        return Err(Error::Message(
            "receive-pack response ended before a complete report-status".to_owned(),
        ));
    }
    Ok(raw)
}

fn receive_pack_read_recoverable(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        ErrorKind::ConnectionReset | ErrorKind::BrokenPipe | ErrorKind::UnexpectedEof
    )
}

/// Whether `raw` (optionally side-band wrapped) contains a full report-status
/// for every ref named in `sent_refs`.
fn receive_pack_response_complete(
    raw: &[u8],
    sent_refs: &[&str],
    server_sideband: bool,
) -> Result<bool> {
    let report = if server_sideband {
        demux_sideband_report(raw, None)?
    } else {
        raw.to_vec()
    };
    Ok(report_lines_complete(&report, sent_refs))
}

/// True when `report` includes an `unpack …` line and an `ok`/`ng` line for
/// every ref the client sent on the wire.
fn report_lines_complete(report: &[u8], sent_refs: &[&str]) -> bool {
    let mut saw_unpack = false;
    let mut per_ref = HashSet::<String>::new();
    let mut cursor = Cursor::new(report);
    while let Ok(Some(pkt)) = pkt_line::read_packet(&mut cursor) {
        let Packet::Data(line) = pkt else {
            continue;
        };
        let line = line.trim_end();
        if line.starts_with("unpack ") {
            saw_unpack = true;
        } else if let Some(refname) = line.strip_prefix("ok ") {
            per_ref.insert(refname.trim().to_owned());
        } else if let Some(rest) = line.strip_prefix("ng ") {
            let (refname, _) = rest.split_once(' ').unwrap_or((rest, ""));
            per_ref.insert(refname.trim().to_owned());
        }
    }
    saw_unpack && sent_refs.iter().all(|r| per_ref.contains(*r))
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

/// Extract band 1 (report-status) from a side-band receive-pack response.
///
/// Bands 2 and 3 are optionally forwarded to `progress`; pass `None` for
/// side-effect-free completeness checks.
fn demux_sideband_report(input: &[u8], mut progress: Option<&mut dyn Progress>) -> Result<Vec<u8>> {
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
            2 | 3 => {
                if let Some(p) = progress.as_deref_mut() {
                    p.message(data);
                }
            }
            _ => {}
        }
    }
    Ok(report)
}

/// Split a side-band stream: band 1 (report-status) is returned; band 2/3
/// (remote diagnostics) is forwarded to `progress`. Lifted from the CLI's
/// `demux_report_and_remote_messages`, but progress goes to the callback rather
/// than directly to stderr (the public API must not assume stdout/stderr).
fn demux_report_and_remote_messages(input: &[u8], progress: &mut dyn Progress) -> Result<Vec<u8>> {
    demux_sideband_report(input, Some(progress))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::objects::ObjectKind;

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
            server_atomic: false,
        }
    }

    #[derive(Default)]
    struct TestConnection {
        caps: Vec<String>,
        refs: Vec<(String, ObjectId)>,
        input: Cursor<Vec<u8>>,
        output: Vec<u8>,
        stateless: bool,
    }

    impl Connection for TestConnection {
        fn reader(&mut self) -> &mut dyn Read {
            &mut self.input
        }
        fn writer(&mut self) -> &mut dyn std::io::Write {
            &mut self.output
        }
        fn advertised_refs(&self) -> &[(String, ObjectId)] {
            &self.refs
        }
        fn capabilities(&self) -> &[String] {
            &self.caps
        }
        fn head_symref(&self) -> Option<&str> {
            None
        }
        fn protocol_version(&self) -> u8 {
            0
        }
        fn stateless_rpc(&self) -> bool {
            self.stateless
        }
    }

    #[test]
    fn advertisement_recognizes_atomic_capability_token() {
        for caps in [
            vec![],
            vec!["atomic"],
            vec!["atomic=false"],
            vec!["atomic", "side-band-64k", "ofs-delta", "push-options"],
        ] {
            let mut conn = TestConnection {
                caps: caps.iter().map(|s| (*s).to_owned()).collect(),
                ..Default::default()
            };
            let adv = AdvertisedState::from_connection(&mut conn);
            assert_eq!(adv.server_atomic, caps.contains(&"atomic"));
            assert_eq!(adv.server_sideband, caps.contains(&"side-band-64k"));
            assert_eq!(adv.server_ofs_delta, caps.contains(&"ofs-delta"));
            assert_eq!(adv.server_push_options, caps.contains(&"push-options"));
        }
    }

    #[test]
    fn command_block_requests_atomic_only_on_first_command_when_supported_and_requested() {
        let new = ObjectId::from_hex(&"1".repeat(40)).unwrap();
        let plan = PushPlan {
            decisions: vec![
                send_decision("refs/heads/one", new),
                send_decision("refs/heads/two", new),
            ],
            to_send: vec![0, 1],
        };
        for supported in [false, true] {
            for requested in [false, true] {
                let mut adv = adv_state(true, true, true);
                adv.server_atomic = supported;
                let opts = PushOptions {
                    atomic: requested,
                    push_options: vec!["ci.skip".to_owned()],
                    ..Default::default()
                };
                let block = build_command_block(&plan, &adv, HashAlgo::Sha1, &opts).unwrap();
                let pkts = decode_block(&block);
                let (command, caps) = pkts[0].as_deref().unwrap().split_once('\0').unwrap();
                assert_eq!(
                    command,
                    format!("{} {} refs/heads/one", "0".repeat(40), new)
                );
                let caps: Vec<_> = caps.split_whitespace().collect();
                assert_eq!(caps.contains(&"atomic"), supported && requested);
                assert!(caps.contains(&"push-options"));
                assert!(caps.contains(&"side-band-64k"));
                assert!(caps.contains(&"object-format=sha1"));
                assert_eq!(
                    pkts[1].as_deref().unwrap(),
                    format!("{} {} refs/heads/two", "0".repeat(40), new)
                );
                assert_eq!(&pkts[2..], &[None, Some("ci.skip".to_owned()), None]);
            }
        }
    }

    #[test]
    fn unsupported_atomic_push_fails_before_any_update_or_pack_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path(), &crate::repo::InitOptions::default()).unwrap();
        let refs = [PushRefSpec {
            src: Some(repo.odb().write(ObjectKind::Blob, b"payload").unwrap()),
            dst: "refs/tags/payload".to_owned(),
            ..Default::default()
        }];
        for stateless in [false, true] {
            for dry_run in [false, true] {
                // Also check no-op pushes: their flush must not precede the error.
                for refs in [&[][..], &refs[..]] {
                    let mut conn = TestConnection {
                        stateless,
                        ..Default::default()
                    };
                    let opts = PushOptions {
                        atomic: true,
                        dry_run,
                        ..Default::default()
                    };
                    let err = push_remote(
                        repo.git_dir(),
                        &mut conn,
                        refs,
                        &opts,
                        &mut crate::fetch::NoProgress,
                        &crate::progress::NeverCancel,
                    )
                    .unwrap_err();
                    assert!(matches!(err, Error::PushAtomicUnsupported), "{err:?}");
                    assert!(
                        conn.output.is_empty(),
                        "unsupported atomic push wrote bytes"
                    );
                    let mut adv = adv_state(false, false, false);
                    adv.server_atomic = true;
                    require_push_capabilities(&adv, &opts).unwrap();
                }
            }
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

    fn flaky_push_repo(
        dir: &Path,
    ) -> (
        Repository,
        std::sync::Arc<crate::odb::store::flaky::FlakyStore>,
    ) {
        let flaky = crate::odb::store::flaky::FlakyStore::new();
        let git_dir = dir.join("repo.git");
        let repo = crate::repo::RepositoryBuilder::new(RepositoryOptions::empty())
            .odb_builder(
                crate::odb::OdbBuilder::files(git_dir.join("objects"))
                    .primary(flaky.clone())
                    .alternates(false),
            )
            .ref_store(std::sync::Arc::new(
                crate::refs::store::MemoryRefStore::new(),
            ))
            .open(&git_dir, None)
            .unwrap();
        (repo, flaky)
    }

    #[test]
    fn push_pack_haves_fail_on_backend_error_instead_of_being_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let (repo, flaky) = flaky_push_repo(dir.path());
        let odb = repo.odb();
        let blob = odb.write(ObjectKind::Blob, b"to send").unwrap();
        let remote_tip = odb.write(ObjectKind::Blob, b"remote has this").unwrap();
        let plan = PushPlan {
            decisions: vec![send_decision("refs/heads/main", blob)],
            to_send: vec![0],
        };
        let mut adv = adv_state(false, true, false);
        adv.remote_refs
            .insert("refs/heads/main".to_owned(), remote_tip);
        assert!(build_push_pack(&plan, &repo, &adv).unwrap().is_some());

        flaky.fail_contains(true);
        let err = build_push_pack(&plan, &repo, &adv).unwrap_err();
        assert!(crate::odb::store::flaky::is_injected(&err), "{err:?}");
    }

    #[test]
    fn push_source_check_fails_on_backend_error_instead_of_missing() {
        let dir = tempfile::tempdir().unwrap();
        let (repo, flaky) = flaky_push_repo(dir.path());
        let src = repo.odb().write(ObjectKind::Blob, b"source").unwrap();
        let spec = PushRefSpec {
            src: Some(src),
            src_ref: None,
            dst: "refs/heads/main".to_owned(),
            force: false,
            delete: false,
            expected_old: None,
            expect_absent: false,
        };
        let decision = decide_push_wire(&spec, &repo, &HashMap::new()).unwrap();
        assert_eq!(decision.new_tip, Some(src));

        flaky.fail_contains(true);
        let err = decide_push_wire(&spec, &repo, &HashMap::new())
            .err()
            .unwrap();
        assert!(crate::odb::store::flaky::is_injected(&err), "{err:?}");
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
        let block = build_command_block(
            &plan,
            &adv_state(false, false, true),
            HashAlgo::Sha1,
            &PushOptions::default(),
        )
        .unwrap();
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
        let opts = PushOptions {
            push_options: vec!["ci.skip".to_owned(), "reviewer=alice".to_owned()],
            ..Default::default()
        };
        let block = build_command_block(&plan, &adv_state(true, true, true), HashAlgo::Sha1, &opts)
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
        let err = require_push_capabilities(&adv_state(true, true, false), &opts).unwrap_err();
        assert!(
            matches!(err, Error::PushOptionsUnsupported),
            "expected PushOptionsUnsupported, got {err:?}"
        );
        assert_eq!(
            err.to_string(),
            "the receiving end does not support push options"
        );
        // Server advertised it: ok.
        require_push_capabilities(&adv_state(true, true, true), &opts).unwrap();
        // No options: ok regardless of capability.
        require_push_capabilities(&adv_state(true, true, false), &PushOptions::default()).unwrap();
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
        impl crate::progress::ProgressSink for Cap {}
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

    #[test]
    fn report_lines_complete_requires_unpack_and_every_sent_ref() {
        let sent = ["refs/heads/main"];
        let full = report_bytes(&["unpack ok", "ok refs/heads/main"]);
        assert!(report_lines_complete(&full, &sent));
        let missing_ok = report_bytes(&["unpack ok"]);
        assert!(!report_lines_complete(&missing_ok, &sent));
        let missing_unpack = report_bytes(&["ok refs/heads/main"]);
        assert!(!report_lines_complete(&missing_unpack, &sent));
        let ng_ok = report_bytes(&["unpack ok", "ng refs/heads/main denied"]);
        assert!(report_lines_complete(&ng_ok, &sent));
    }

    struct AbruptAfterRead {
        payload: Vec<u8>,
        pos: usize,
        err_kind: ErrorKind,
    }

    impl Read for AbruptAfterRead {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.pos < self.payload.len() {
                let n = std::cmp::min(buf.len(), self.payload.len() - self.pos);
                buf[..n].copy_from_slice(&self.payload[self.pos..self.pos + n]);
                self.pos += n;
                return Ok(n);
            }
            Err(std::io::Error::new(self.err_kind, "forcibly closed"))
        }
    }

    fn sideband_receive_pack_wire(hook_line: &[u8]) -> Vec<u8> {
        let report_body = report_bytes(&["unpack ok", "ok refs/heads/main"]);
        let mut band1 = vec![1u8];
        band1.extend_from_slice(&report_body);
        let mut wire = Vec::new();
        pkt_line::write_packet_raw(&mut wire, &band1).unwrap();
        let mut band2 = vec![2u8];
        band2.extend_from_slice(hook_line);
        pkt_line::write_packet_raw(&mut wire, &band2).unwrap();
        wire.extend_from_slice(b"0000");
        wire
    }

    struct EofAfterRead {
        payload: Vec<u8>,
        pos: usize,
    }

    impl Read for EofAfterRead {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.pos < self.payload.len() {
                let n = std::cmp::min(buf.len(), self.payload.len() - self.pos);
                buf[..n].copy_from_slice(&self.payload[self.pos..self.pos + n]);
                self.pos += n;
                Ok(n)
            } else {
                Ok(0)
            }
        }
    }

    /// Mirrors `push_remote`: read the wire response, then demux side-band once.
    fn read_push_report(
        reader: &mut dyn Read,
        sent_refs: &[&str],
        sideband: bool,
        progress: &mut dyn Progress,
    ) -> Result<Vec<u8>> {
        let raw = read_receive_pack_response(reader, sent_refs, sideband)?;
        if sideband {
            demux_report_and_remote_messages(&raw, progress)
        } else {
            Ok(raw)
        }
    }

    #[test]
    fn read_receive_pack_response_tolerates_reset_after_full_report() {
        let report = report_bytes(&["unpack ok", "ok refs/heads/main"]);
        let mut reader = AbruptAfterRead {
            payload: report.clone(),
            pos: 0,
            err_kind: ErrorKind::ConnectionReset,
        };
        let out =
            read_receive_pack_response(&mut reader, &["refs/heads/main"], false).expect("reset");
        assert_eq!(out, report);
    }

    #[test]
    fn read_receive_pack_response_fails_reset_on_incomplete_report() {
        let partial = report_bytes(&["unpack ok"]);
        let mut reader = AbruptAfterRead {
            payload: partial,
            pos: 0,
            err_kind: ErrorKind::ConnectionReset,
        };
        let err = read_receive_pack_response(&mut reader, &["refs/heads/main"], false).unwrap_err();
        assert!(
            matches!(err, Error::Io(_)),
            "incomplete report must stay an I/O error, got {err:?}"
        );
    }

    #[test]
    fn sideband_remote_messages_flush_unterminated_hook_text() {
        struct Noop;
        impl crate::progress::ProgressSink for Noop {}
        impl Progress for Noop {}
        let mut inner = Noop;
        let mut sideband = PushSidebandCollector {
            inner: &mut inner,
            remote_messages: Vec::new(),
            pending: SidebandLines::new(),
        };
        let hook = b"policy: no ticket";
        let wire = sideband_receive_pack_wire(hook);
        demux_report_and_remote_messages(&wire, &mut sideband).unwrap();
        sideband.finish_pending();
        assert_eq!(
            sideband.remote_messages,
            vec!["policy: no ticket".to_owned()]
        );
    }

    #[test]
    fn sideband_diagnostics_delivered_once_on_clean_eof() {
        struct Cap(Vec<u8>);
        impl crate::progress::ProgressSink for Cap {}
        impl Progress for Cap {
            fn message(&mut self, bytes: &[u8]) {
                self.0.extend_from_slice(bytes);
            }
        }
        let hook = b"hook output\n";
        let wire = sideband_receive_pack_wire(hook);
        let mut reader = EofAfterRead {
            payload: wire,
            pos: 0,
        };
        let mut cap = Cap(Vec::new());
        read_push_report(&mut reader, &["refs/heads/main"], true, &mut cap)
            .expect("clean eof side-band push report");
        assert_eq!(cap.0.as_slice(), hook);
    }

    #[test]
    fn sideband_diagnostics_delivered_once_on_connection_reset() {
        struct Cap(Vec<u8>);
        impl crate::progress::ProgressSink for Cap {}
        impl Progress for Cap {
            fn message(&mut self, bytes: &[u8]) {
                self.0.extend_from_slice(bytes);
            }
        }
        let hook = b"hook output\n";
        let wire = sideband_receive_pack_wire(hook);
        let mut reader = AbruptAfterRead {
            payload: wire,
            pos: 0,
            err_kind: ErrorKind::ConnectionReset,
        };
        let mut cap = Cap(Vec::new());
        read_push_report(&mut reader, &["refs/heads/main"], true, &mut cap)
            .expect("reset after complete side-band report");
        assert_eq!(cap.0.as_slice(), hook);
    }

    /// Initial push of one branch tip must not disable deltas just because there
    /// is a single want (regression for #1941).
    #[test]
    fn push_pack_initial_branch_push_uses_deltas() {
        use std::process::Command;

        fn pack_delta_count(bytes: &[u8], algo_len: usize) -> usize {
            fn read_type_size(b: &[u8]) -> (u8, usize, usize) {
                let type_code = (b[0] >> 4) & 7;
                let mut size = (b[0] & 0x0f) as usize;
                let mut shift = 4;
                let mut i = 1;
                while b[i - 1] & 0x80 != 0 {
                    size |= ((b[i] & 0x7f) as usize) << shift;
                    shift += 7;
                    i += 1;
                }
                (type_code, size, i)
            }
            fn zlib_consume(b: &[u8]) -> usize {
                use std::io::Read as _;
                let mut dec = flate2::bufread::ZlibDecoder::new(b);
                let mut sink = Vec::new();
                dec.read_to_end(&mut sink).expect("zlib");
                dec.total_in() as usize
            }
            let nr = u32::from_be_bytes(bytes[8..12].try_into().unwrap()) as usize;
            let mut pos = 12usize;
            let mut deltas = 0usize;
            for _ in 0..nr {
                let (type_code, _, consumed) = read_type_size(&bytes[pos..]);
                pos += consumed;
                match type_code {
                    6 | 7 => {
                        deltas += 1;
                        if type_code == 7 {
                            pos += algo_len;
                        } else {
                            while bytes[pos] & 0x80 != 0 {
                                pos += 1;
                            }
                            pos += 1;
                        }
                    }
                    _ => {}
                }
                pos += zlib_consume(&bytes[pos..]);
            }
            deltas
        }

        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        for i in 0..40 {
            if i == 0 {
                Command::new("git")
                    .current_dir(dir)
                    .args(["init", "-q", "-b", "main", "."])
                    .env("GIT_CONFIG_GLOBAL", "/dev/null")
                    .env("GIT_CONFIG_SYSTEM", "/dev/null")
                    .status()
                    .unwrap();
            }
            std::fs::write(dir.join(format!("f{i:03}.txt")), format!("content {i}\n")).unwrap();
            std::fs::write(dir.join("shared.txt"), format!("shared rev {i}\n")).unwrap();
            Command::new("git")
                .current_dir(dir)
                .args(["add", "-A"])
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .status()
                .unwrap();
            Command::new("git")
                .current_dir(dir)
                .args(["commit", "-q", "-m", &format!("commit {i}")])
                .env("GIT_AUTHOR_NAME", "T")
                .env("GIT_AUTHOR_EMAIL", "t@example.com")
                .env("GIT_AUTHOR_DATE", "2005-04-07T22:13:13 +0200")
                .env("GIT_COMMITTER_NAME", "T")
                .env("GIT_COMMITTER_EMAIL", "t@example.com")
                .env("GIT_COMMITTER_DATE", "2005-04-07T22:13:13 +0200")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .status()
                .unwrap();
        }
        Command::new("git")
            .current_dir(dir)
            .args(["repack", "-adf"])
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .status()
            .unwrap();
        let git_dir = dir.join(".git");
        let repo = Repository::open(&git_dir, &RepositoryOptions::empty()).unwrap();
        let odb = repo.odb();
        let tip = crate::refs::resolve_ref(&git_dir, "HEAD").expect("head");
        let plan = PushPlan {
            decisions: vec![send_decision("refs/heads/main", tip)],
            to_send: vec![0],
        };
        let adv = adv_state(false, true, false);
        let pack = build_push_pack(&plan, &repo, &adv)
            .unwrap()
            .expect("pack bytes");
        let deltas = pack_delta_count(&pack, odb.hash_algo().len());
        assert!(
            deltas > 0,
            "initial push pack must contain delta objects (got {deltas})"
        );
    }
}
