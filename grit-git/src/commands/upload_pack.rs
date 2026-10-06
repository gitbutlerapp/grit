//! `grit upload-pack` — send objects for fetch (server side).
//!
//! Invoked on the remote side of a fetch. Advertises refs in pkt-line format,
//! negotiates want/have (protocol v0, `multi_ack_detailed`), then streams a
//! packfile (side-band-64k) to the client.

use anyhow::{Context, Result};
use clap::Args as ClapArgs;
use grit_lib::config::ConfigSet;
use grit_lib::merge_base;
use grit_lib::objects::{parse_commit, parse_tag, parse_tree, ObjectId, ObjectKind};
use grit_lib::ref_namespace;
use grit_lib::refs;
use grit_lib::repo::Repository;
use grit_lib::state::resolve_head;
use grit_lib::state::HeadState;
use std::collections::HashSet;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use crate::commands::serve_v2::{serve_loop, ServerCaps};
use crate::protocol_wire;
use crate::trace2_transfer;
use grit_lib::pkt_line;

/// Arguments for `grit upload-pack`.
#[derive(Debug, ClapArgs)]
#[command(about = "Send objects for fetch (server side)")]
pub struct Args {
    /// Path to the repository (bare or non-bare).
    #[arg(value_name = "DIRECTORY")]
    pub directory: PathBuf,

    /// Only advertise refs and capabilities, then exit.
    #[arg(long)]
    pub advertise_refs: bool,

    /// Smart-HTTP discovery mode; equivalent to advertising refs for git-http-backend.
    #[arg(long = "http-backend-info-refs", hide = true)]
    pub http_backend_info_refs: bool,

    /// Smart-HTTP stateless RPC mode; accepted for compatibility.
    #[arg(long = "stateless-rpc", hide = true)]
    pub stateless_rpc: bool,

    /// Compatibility flag accepted by Git's upload-pack.
    #[arg(long, hide = true)]
    pub strict: bool,
}

pub fn run(args: Args) -> Result<()> {
    // Match `git upload-pack`: default `GIT_NO_LAZY_FETCH=1` so remote `pack-objects` does not
    // lazy-fetch missing blobs (t0411-clone-from-partial, promisor clone via upload-pack).
    if std::env::var("GIT_NO_LAZY_FETCH")
        .ok()
        .map(|s| s.trim().is_empty())
        .unwrap_or(true)
    {
        std::env::set_var("GIT_NO_LAZY_FETCH", "1");
    }

    let repo = open_repo(&args.directory).with_context(|| {
        format!(
            "could not open repository at '{}'",
            args.directory.display()
        )
    })?;
    repo.enforce_safe_directory_git_dir()?;
    let config = ConfigSet::load(Some(&repo.git_dir), false).unwrap_or_default();
    grit_lib::upload_filter::validate_upload_filter_config(&config)?;

    trace2_transfer::emit_negotiated_version_from_git_protocol_env();

    let server_proto = protocol_wire::server_protocol_version_from_git_protocol_env();
    let advertise_only = args.advertise_refs || args.http_backend_info_refs;

    if server_proto == 2 {
        let caps = ServerCaps::load(&repo.git_dir);
        if advertise_only {
            let mut out = io::stdout();
            caps.advertise(&mut out)?;
            out.flush()?;
            return Ok(());
        }
        let stdin = io::stdin();
        let mut input = stdin.lock();
        let stdout = io::stdout();
        let mut out = stdout.lock();
        if !args.stateless_rpc {
            caps.advertise(&mut out)?;
            out.flush()?;
        }
        drop(out);
        if args.stateless_rpc {
            // Smart-HTTP: the backend re-invokes upload-pack per negotiation round, so process a
            // single request statelessly (when the server becomes `ready`, end the response after
            // the `acknowledgments` section instead of streaming the pack inline).
            let _ = crate::commands::serve_v2::process_one_v2_request(
                &mut input,
                &repo.git_dir,
                &caps,
                true,
            )?;
            return Ok(());
        }
        return serve_loop(&mut input, &repo.git_dir, &caps);
    }

    if advertise_only {
        return advertise_refs_with_caps(&repo, server_proto);
    }

    let mut out = io::stdout();
    if !args.stateless_rpc {
        if server_proto == 1 {
            pkt_line::write_line(&mut out, "version 1")?;
            out.flush()?;
        }
        write_ref_advertisement(&mut out, &repo.git_dir)?;
        pkt_line::write_flush(&mut out)?;
        out.flush()?;
    }

    let mut stdin = io::stdin();
    let mut wants: Vec<ObjectId> = Vec::new();
    let mut client_shallow_boundaries: HashSet<ObjectId> = HashSet::new();
    let mut requested_depth: Option<usize> = None;
    let mut filter_spec: Option<String> = None;
    let mut multi_ack_detailed = false;
    let mut no_done = false;
    let mut no_progress = false;
    loop {
        match pkt_line::read_packet(&mut stdin)? {
            None => break,
            Some(pkt_line::Packet::Flush) => break,
            Some(pkt_line::Packet::Data(line)) => {
                if let Some(rest) = line.strip_prefix("want ") {
                    let hex = rest.split_whitespace().next().unwrap_or(rest);
                    let features = rest.strip_prefix(hex).unwrap_or("").trim();
                    if wants.is_empty() && features.contains("multi_ack_detailed") {
                        multi_ack_detailed = true;
                    }
                    // `no-done` (only honored with `multi_ack_detailed`) lets the server skip the
                    // client's final `done`: when it sends `ACK <oid> ready` at a flush it follows
                    // with `ACK <oid>` and streams the pack in the same stateless RPC, so the client
                    // never has to send `done` (upload-pack.c get_common_commits, t5539 test 3).
                    if wants.is_empty() && features.split_whitespace().any(|f| f == "no-done") {
                        no_done = true;
                    }
                    if wants.is_empty() && features.split_whitespace().any(|f| f == "no-progress") {
                        no_progress = true;
                    }
                    if wants.is_empty() {
                        if let Some(sid) = trace2_transfer::extract_session_id_feature(features) {
                            trace2_transfer::emit_client_sid(sid);
                        }
                    }
                    if let Ok(oid) = ObjectId::from_hex(hex) {
                        wants.push(oid);
                    }
                } else if let Some(rest) = line.strip_prefix("shallow ") {
                    let hex = rest.trim();
                    if let Ok(oid) = ObjectId::from_hex(hex) {
                        client_shallow_boundaries.insert(oid);
                    }
                } else if let Some(rest) = line.strip_prefix("deepen ") {
                    let depth = rest.trim().parse::<usize>().unwrap_or(0);
                    // `fetch --unshallow` uses a sentinel depth (`2147483647`) that should not
                    // impose an artificial depth limit.
                    if depth > 0 && depth < i32::MAX as usize {
                        requested_depth = Some(match requested_depth {
                            Some(current) => current.min(depth),
                            None => depth,
                        });
                    }
                } else if let Some(rest) = line.strip_prefix("filter ") {
                    let spec = rest.trim();
                    if !spec.is_empty() {
                        grit_lib::upload_filter::validate_upload_filter_request(&config, spec)?;
                        filter_spec = Some(spec.to_owned());
                    }
                }
            }
            _ => {}
        }
    }
    if wants.is_empty() {
        return Ok(());
    }

    // Fetch clients may send the same `want` OID twice (e.g. duplicate pkt-lines). `pack-objects
    // --revs` treats each positive rev line as a separate walk root; duplicates corrupt the pack.
    let mut want_unique: Vec<ObjectId> = Vec::new();
    let mut want_seen: HashSet<ObjectId> = HashSet::new();
    for w in wants {
        if want_seen.insert(w) {
            want_unique.push(w);
        }
    }

    // Validate that every `want` is something we are allowed to serve: either a tip of an
    // advertised (non-hidden) ref, or — when `uploadpack.allow{Tip,Reachable,Any}SHA1InWant` is
    // set — an existing object (optionally reachable from a ref). A `want` for an object we do not
    // have, or for a non-tip object the policy forbids, draws an `ERR upload-pack: not our ref`
    // packet on stdout plus a matching error on stderr and exit 128 (t5530 bad-want subtests).
    let allow_tip = config_bool(&config, "uploadpack.allowtipsha1inwant");
    let allow_reachable = config_bool(&config, "uploadpack.allowreachablesha1inwant");
    let allow_any = config_bool(&config, "uploadpack.allowanysha1inwant");
    if !allow_any {
        // Advertised (non-hidden) tips are always fetchable. `allow_tip` additionally permits any
        // ref tip including those hidden by `transfer.hideRefs` / `uploadpack.hideRefs`.
        let advertised_tips = advertised_ref_oids(&repo.git_dir, &config);
        let all_tips = our_ref_oids(&repo.git_dir);
        for w in &want_unique {
            if advertised_tips.contains(w) {
                continue;
            }
            let exists = repo.odb.read(w).is_ok();
            if !exists {
                // Object not present at all — never our ref regardless of policy.
                return reject_not_our_ref(&mut out, w);
            }
            if allow_tip && all_tips.contains(w) {
                // `allow-tip-sha1-in-want`: a want for *any* ref tip (incl. hidden) is acceptable,
                // but a non-tip object is not — it must still pass the reachable/reject checks
                // below (t5516 'deny fetch unreachable SHA1, allowtipsha1inwant=true').
                continue;
            }
            if allow_reachable && is_reachable_from_our_refs(&repo, &all_tips, w) {
                continue;
            }
            // `check_non_tip`: without allow-reachable, a non-stateless client cannot legitimately
            // ask for a non-tip object, so reject immediately. A stateless client's choice may be
            // based on a stale advertisement, so it is given the benefit of a reachability check —
            // but with the default (deny) policy we still reject unreachable non-tips.
            if !args.stateless_rpc || !is_reachable_from_our_refs(&repo, &all_tips, w) {
                return reject_not_our_ref(&mut out, w);
            }
        }
    }

    let want_set: HashSet<ObjectId> = want_unique.iter().copied().collect();

    let mut got_common = false;
    let mut got_other = false;
    let mut last_hex = String::new();
    let mut client_known: HashSet<ObjectId> = HashSet::new();
    let mut client_have_commits: Vec<ObjectId> = Vec::new();
    // Distinct server-known `have` objects, mirroring `upload-pack.c`'s `have_obj` array. Each OID
    // sets its `THEY_HAVE` flag once; the non-multi-ack ACK is sent whenever `have_obj.nr == 1`, so
    // a repeated `have` for the same (and only) object is ACKed each time (t5530 protocol-v0 ACKs
    // repeated non-commit objects repeatedly).
    let mut they_have: HashSet<ObjectId> = HashSet::new();
    let mut have_obj_count: usize = 0;

    // For a stateless client, EOF immediately after the want/shallow/deepen flush is acceptable: it
    // consumes the shallow list and re-issues the haves in a later RPC. Emit the shallow-list
    // response (if any) before negotiating so the client can read it (t5530 EOF just after
    // stateless client wants).
    if args.stateless_rpc {
        emit_v0_shallow_list(
            &mut out,
            &repo,
            &want_unique,
            &client_shallow_boundaries,
            requested_depth,
        )?;
        out.flush()?;
    }

    // Whether the client actually negotiated (sent any `have`/`done`). A stateless client that
    // closes its input right after the want/shallow/deepen flush must not trigger pack generation;
    // it will resume with haves in a follow-up RPC.
    let mut saw_negotiation = false;

    loop {
        match pkt_line::read_packet(&mut stdin)? {
            None => break,
            Some(pkt_line::Packet::Flush) => {
                let mut sent_ready = false;
                if multi_ack_detailed
                    && got_common
                    && !got_other
                    && ok_to_give_up(&repo, &want_set, &client_known)
                {
                    pkt_line::write_line(&mut out, &format!("ACK {last_hex} ready"))?;
                    sent_ready = true;
                }
                if args.stateless_rpc {
                    // Stateless negotiation ends at a flush: mirror `get_common_commits` —
                    // write NAK only when no server-known `have` was received (or multi-ack).
                    if have_obj_count == 0 || multi_ack_detailed {
                        pkt_line::write_line(&mut out, "NAK")?;
                    }
                    // `no-done` short-circuit: once `ACK <oid> ready` has been sent, the server
                    // does not wait for the client's `done`. It sends a final `ACK <oid>` and
                    // streams the pack in this same RPC (upload-pack.c: `no_done && sent_ready`),
                    // so the client never sends `done` (t5539 "no shallow lines after ACK ready").
                    if no_done && sent_ready {
                        pkt_line::write_line(&mut out, &format!("ACK {last_hex}"))?;
                        out.flush()?;
                        break;
                    }
                    // Otherwise the client re-sends its haves in a later RPC (t5530 ACKs repeated
                    // non-commit objects; EOF after stateless wants).
                    out.flush()?;
                    return Ok(());
                }
                if got_common || multi_ack_detailed {
                    pkt_line::write_line(&mut out, "NAK")?;
                }
                got_common = false;
                got_other = false;
                out.flush()?;
            }
            Some(pkt_line::Packet::Data(line)) => {
                if line == "done" {
                    saw_negotiation = true;
                    if (!last_hex.is_empty() && multi_ack_detailed) || got_common {
                        pkt_line::write_line(&mut out, &format!("ACK {last_hex}"))?;
                    } else {
                        pkt_line::write_line(&mut out, "NAK")?;
                    }
                    out.flush()?;
                    break;
                }
                if let Some(rest) = line.strip_prefix("filter ") {
                    let spec = rest.trim();
                    if !spec.is_empty() {
                        grit_lib::upload_filter::validate_upload_filter_request(&config, spec)?;
                        filter_spec = Some(spec.to_owned());
                    }
                    continue;
                }
                if let Some(hex) = line.strip_prefix("have ").map(str::trim) {
                    if let Ok(oid) = ObjectId::from_hex(hex) {
                        saw_negotiation = true;
                        if repo.odb.read(&oid).is_err() {
                            got_other = true;
                            if multi_ack_detailed && ok_to_give_up(&repo, &want_set, &client_known)
                            {
                                pkt_line::write_line(
                                    &mut out,
                                    &format!("ACK {} continue", oid.to_hex()),
                                )?;
                            }
                        } else {
                            got_common = true;
                            last_hex = oid.to_hex();
                            // A `have` may name any object type. Only commits contribute ancestor
                            // history and pack-exclusion roots; trees/blobs are recorded purely so
                            // they can be ACKed (t5530 repeated non-commit `have`s).
                            let is_commit = matches!(
                                repo.odb.read(&oid).map(|o| o.kind),
                                Ok(ObjectKind::Commit)
                            );
                            if is_commit {
                                client_have_commits.push(oid);
                                merge_ancestors_into(
                                    &repo,
                                    oid,
                                    &mut client_known,
                                    Some(&client_shallow_boundaries),
                                )?;
                            } else {
                                client_known.insert(oid);
                            }
                            // Mirror `do_got_oid`: each object increments `have_obj.nr` only the
                            // first time it is seen. The non-multi-ack ACK fires whenever the count
                            // is exactly 1, so a single repeated object is ACKed every time.
                            if they_have.insert(oid) {
                                have_obj_count += 1;
                            }
                            if multi_ack_detailed {
                                pkt_line::write_line(&mut out, &format!("ACK {last_hex} common"))?;
                            } else if have_obj_count == 1 {
                                pkt_line::write_line(&mut out, &format!("ACK {last_hex}"))?;
                            }
                        }
                    }
                    out.flush()?;
                }
            }
            _ => {}
        }
    }

    // A stateless client that closed its input right after the want/shallow/deepen flush (without
    // sending any `have`/`done`) only wanted the shallow list; do not generate a pack. The client
    // resumes negotiation with haves in a follow-up RPC (t5530 EOF just after stateless wants).
    if args.stateless_rpc && !saw_negotiation {
        out.flush()?;
        return Ok(());
    }

    // Only short-circuit to an empty pack when every `want` is a commit the client already has.
    // `client_known` includes blob OIDs reachable from `have` commits (server-side walk), but a
    // partial-clone client may still lack those blobs — never treat a blob/tree `want` as
    // satisfied by that set (t0410 lazy fetch).
    let already_have_all = wants_include_only_commits(&repo, &want_unique)
        && want_unique.iter().all(|w| client_known.contains(w));
    if already_have_all {
        let pack = crate::pack_objects_upload::empty_packfile_v2_bytes();
        crate::pack_objects_upload::write_sideband_64k(&mut out, &pack)?;
    } else {
        let mut exclusion_commits: Vec<ObjectId> = if client_shallow_boundaries.is_empty() {
            client_have_commits.clone()
        } else {
            // With shallow clients, `have` commit closure past a shallow boundary is incomplete.
            // Avoid excluding those unseen ancestors from the generated pack.
            Vec::new()
        };
        // For a depth-limited request, cut the parent chains of the boundary commits via
        // `--shallow <oid>` rather than excluding the boundary's *parents* via `--not`. A plain
        // `--not <parent>` exclusion drops every object reachable from the cut-off history,
        // including trees/blobs still referenced by an in-depth commit (e.g. a file added in the
        // very first commit and never changed). That yields a corrupt shallow pack whose refs
        // point at objects with missing blobs, which `git fsck` rejects
        // (t5537 "shallow fetches check connectivity before writing shallow file").
        let shallow_commits: Vec<ObjectId> = if let Some(depth) = requested_depth {
            crate::pack_objects_upload::compute_depth_boundary_commits(&repo, &want_unique, depth)?
        } else {
            Vec::new()
        };
        exclusion_commits.sort_by_key(|oid| oid.to_hex());
        exclusion_commits.dedup();
        // Thin packs subtract the full closure of `have` commits. That is only safe when every
        // `want` is a commit OID; blob/tree lazy-fetch wants must use a self-contained pack
        // (t0410 partial-clone explicit wants).
        let thin = client_shallow_boundaries.is_empty()
            && !exclusion_commits.is_empty()
            && wants_include_only_commits(&repo, &want_unique);
        let advertises_promisor = config_bool(&config, "promisor.advertise");
        let omit_missing_promisor = filter_spec.is_some() && advertises_promisor;
        // `upload-pack` pins `GIT_NO_LAZY_FETCH=1` by default, so the server-side `pack-objects`
        // never lazily fetches missing objects — it just fails if it cannot read an object
        // (t0411-clone-from-partial: a plain clone/fetch from a partial-clone server must NOT run
        // the promisor's upload-pack). Back-fill omitted blobs only when the operator explicitly
        // re-enabled lazy fetching (`GIT_NO_LAZY_FETCH=0`).
        let lazy_fetch_enabled =
            !crate::commands::promisor_hydrate::git_no_lazy_fetch_env_disables_lazy()
                .unwrap_or(false);
        let force_lazy_fetch = lazy_fetch_enabled
            && !advertises_promisor
            && (filter_spec.is_some() || !exclusion_commits.is_empty());
        if force_lazy_fetch && !exclusion_commits.is_empty() {
            hydrate_upload_pack_blobs_missing_from_client(&repo, &want_unique, &exclusion_commits)?;
        }
        let mut child = crate::pack_objects_upload::spawn_pack_objects_upload_shallow(
            &repo.git_dir,
            thin,
            filter_spec.as_deref(),
            !shallow_commits.is_empty(),
            !no_progress,
            omit_missing_promisor,
            force_lazy_fetch,
        )?;
        {
            let mut pin = child.stdin.take().context("pack-objects stdin")?;
            crate::pack_objects_upload::write_pack_objects_revs_stdin_shallow(
                &mut pin,
                &want_unique,
                &exclusion_commits,
                &shallow_commits,
            )?;
        }
        crate::pack_objects_upload::drain_pack_objects_child(child, &mut out, true)?;
    }

    pkt_line::write_flush(&mut out)?;
    out.flush()?;
    Ok(())
}

/// Read a boolean config value (default `false`).
fn config_bool(config: &ConfigSet, key: &str) -> bool {
    config.get_bool(key).and_then(|r| r.ok()).unwrap_or(false)
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

/// Emit the protocol-v0 shallow-list response (`shallow`/`unshallow` lines + flush) for a
/// depth-limited request, mirroring `upload-pack.c`'s `send_shallow_list`/`packet_flush(1)`. Only
/// a `deepen <n>` request produces a flush here; without deepening there is nothing to send.
fn emit_v0_shallow_list(
    out: &mut impl Write,
    repo: &Repository,
    wants: &[ObjectId],
    client_shallow: &HashSet<ObjectId>,
    requested_depth: Option<usize>,
) -> Result<()> {
    let Some(depth) = requested_depth else {
        return Ok(());
    };
    let client_shallow_vec: Vec<ObjectId> = client_shallow.iter().copied().collect();
    let new_shallow = grit_lib::rev_list::shallow_grafts_for_upload_pack_deepen(
        repo,
        wants,
        &client_shallow_vec,
        depth,
    );
    let new_shallow_set: HashSet<ObjectId> = new_shallow.iter().copied().collect();
    for oid in &new_shallow {
        if !client_shallow.contains(oid) {
            pkt_line::write_line(out, &format!("shallow {}", oid.to_hex()))?;
        }
    }
    // A client-declared shallow commit is `unshallow`ed only once the deepened history reaches past
    // it — i.e. all of its parents are now within the fetched depth (matching `send_unshallow`,
    // which emits only commits flagged `NOT_SHALLOW`). A commit that remains the depth boundary
    // (its parents are still cut off) keeps its shallow status and emits nothing (t5530 deepen 1).
    let included = commits_within_depth(repo, wants, depth);
    for oid in &client_shallow_vec {
        if new_shallow_set.contains(oid) {
            continue;
        }
        if !included.contains(oid) {
            continue;
        }
        let parents = commit_parents(repo, oid);
        let interior = !parents.is_empty() && parents.iter().all(|p| included.contains(p));
        if interior {
            pkt_line::write_line(out, &format!("unshallow {}", oid.to_hex()))?;
        }
    }
    pkt_line::write_flush(out)?;
    Ok(())
}

/// Parent OIDs of a commit (empty if `oid` is missing or not a commit).
fn commit_parents(repo: &Repository, oid: &ObjectId) -> Vec<ObjectId> {
    match repo.odb.read(oid) {
        Ok(obj) if obj.kind == ObjectKind::Commit => parse_commit(&obj.data)
            .map(|c| c.parents)
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

/// BFS the commit graph from `wants`, returning every commit reachable within `depth` generations
/// (the want itself is depth 1). Mirrors `commits_within_parent_depth` used by the shallow-graft
/// machinery so the v0 shallow-list response agrees with the generated pack.
fn commits_within_depth(repo: &Repository, wants: &[ObjectId], depth: usize) -> HashSet<ObjectId> {
    use std::collections::VecDeque;
    let mut best: std::collections::HashMap<ObjectId, usize> = std::collections::HashMap::new();
    let mut q: VecDeque<(ObjectId, usize)> = VecDeque::new();
    for &w in wants {
        best.insert(w, 1);
        q.push_back((w, 1));
    }
    while let Some((oid, d)) = q.pop_front() {
        if best.get(&oid).copied() != Some(d) || d >= depth {
            continue;
        }
        for p in commit_parents(repo, &oid) {
            let nd = d + 1;
            if nd > depth {
                continue;
            }
            if best.get(&p).copied().unwrap_or(usize::MAX) > nd {
                best.insert(p, nd);
                q.push_back((p, nd));
            }
        }
    }
    best.into_keys().collect()
}

/// Collect the OIDs that are tips of advertised (non-hidden) refs, i.e. the objects a client is
/// allowed to `want` by default. This mirrors upstream `mark_our_ref`: HEAD plus every ref under
/// `refs/`.
fn our_ref_oids(git_dir: &Path) -> HashSet<ObjectId> {
    let mut set: HashSet<ObjectId> = HashSet::new();
    if let Ok(oid) = refs::resolve_ref(git_dir, "HEAD") {
        set.insert(oid);
    }
    if let Ok(entries) = refs::list_refs(git_dir, "refs/") {
        for (_name, oid) in entries {
            set.insert(oid);
        }
    }
    set
}

/// Collect the OIDs of tips that are actually *advertised* — every ref under `refs/` (plus HEAD)
/// minus those hidden by `transfer.hideRefs` / `uploadpack.hideRefs`. These are the objects a
/// default-policy client may `want` without any `allow-*-sha1-in-want` capability (mirrors
/// upstream `mark_our_ref` over the advertised set).
fn advertised_ref_oids(git_dir: &Path, config: &ConfigSet) -> HashSet<ObjectId> {
    let mut hidden = grit_lib::ref_exclusions::RefExclusions::default();
    hidden.load_hidden_refs_from_config(config, "uploadpack");
    let mut set: HashSet<ObjectId> = HashSet::new();
    if let Ok(oid) = refs::resolve_ref(git_dir, "HEAD") {
        set.insert(oid);
    }
    if let Ok(entries) = refs::list_refs(git_dir, "refs/") {
        for (name, oid) in entries {
            if hidden.ref_excluded(Some(&name), &name) {
                continue;
            }
            set.insert(oid);
        }
    }
    set
}

/// Whether `oid` is reachable (as a commit ancestor) from any of our advertised ref tips. Used for
/// the `allow-reachable-sha1-in-want` policy and the stateless non-tip tolerance check.
fn is_reachable_from_our_refs(
    repo: &Repository,
    our_refs: &HashSet<ObjectId>,
    oid: &ObjectId,
) -> bool {
    for tip in our_refs {
        if let Ok(reachable) = merge_base::ancestor_closure(repo, *tip) {
            if reachable.contains(oid) {
                return true;
            }
        }
    }
    false
}

/// Send `ERR upload-pack: not our ref <oid>` on stdout, emit a matching `error:` line on stderr,
/// flush, and exit 128 — matching `upload-pack.c` `check_non_tip` / `parse_want` rejection.
fn reject_not_our_ref(out: &mut impl Write, oid: &ObjectId) -> Result<()> {
    let hex = oid.to_hex();
    pkt_line::write_line(out, &format!("ERR upload-pack: not our ref {hex}"))?;
    out.flush()?;
    eprintln!("error: git upload-pack: not our ref {hex}");
    std::process::exit(128);
}

/// Returns `true` when every wanted OID resolves to a commit object in the server ODB.
fn wants_include_only_commits(repo: &Repository, wants: &[ObjectId]) -> bool {
    for w in wants {
        let Ok(obj) = repo.odb.read(w) else {
            return false;
        };
        if obj.kind != ObjectKind::Commit {
            return false;
        }
    }
    true
}

fn merge_ancestors_into(
    repo: &Repository,
    tip: ObjectId,
    into: &mut HashSet<ObjectId>,
    shallow_boundaries: Option<&HashSet<ObjectId>>,
) -> Result<()> {
    let boundaries = match shallow_boundaries {
        Some(b) if !b.is_empty() => b,
        _ => {
            let anc = merge_base::ancestor_closure(repo, tip)?;
            into.extend(anc);
            return Ok(());
        }
    };
    let mut stack = vec![tip];
    let mut seen = HashSet::new();
    while let Some(oid) = stack.pop() {
        if !seen.insert(oid) {
            continue;
        }
        into.insert(oid);
        if boundaries.contains(&oid) {
            continue;
        }
        let Ok(obj) = repo.odb.read(&oid) else {
            continue;
        };
        if obj.kind != ObjectKind::Commit {
            continue;
        }
        let commit = parse_commit(&obj.data)?;
        stack.extend(commit.parents);
    }
    Ok(())
}

fn ok_to_give_up(
    _repo: &Repository,
    wants: &HashSet<ObjectId>,
    client_known: &HashSet<ObjectId>,
) -> bool {
    // Match `upload-pack.c` `ok_to_give_up`: we can stop when the client already has every wanted
    // commit (not merely an ancestor of each want).
    !client_known.is_empty() && wants.iter().all(|w| client_known.contains(w))
}

/// Hex string for the null OID, sized to the object format (40 zeros for SHA-1, 64 for SHA-256).
fn zero_oid_hex_for_format(object_format: &str) -> String {
    let width = if object_format.eq_ignore_ascii_case("sha256") {
        64
    } else {
        40
    };
    "0".repeat(width)
}

fn write_ref_advertisement(w: &mut impl Write, git_dir: &Path) -> Result<()> {
    let version = crate::version_string();
    let set = ConfigSet::load(Some(git_dir), false).unwrap_or_default();
    let object_format = set
        .get("extensions.objectformat")
        .or_else(|| set.get("extensions.objectFormat"))
        .map(|s| s.to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "sha1".to_owned());

    // `transfer.hideRefs` / `uploadpack.hideRefs`: refs matching these patterns are not advertised
    // (and not made wantable). Patterns prefixed `^` match the full storage name; otherwise the
    // namespace-stripped (advertised) name (matches Git `ref_is_hidden`).
    let hide = grit_lib::hide_refs::hide_ref_patterns_uploadpack(&set);

    // Resolve HEAD relative to the active namespace. Under `GIT_NAMESPACE`, only the namespaced
    // `refs/namespaces/<ns>/HEAD` is consulted (Git's `head_ref_namespaced`); a missing namespaced
    // HEAD means no `HEAD` line (and no `symref=HEAD:` capability) is advertised.
    let head_symref_logical = refs::read_symbolic_ref(git_dir, "HEAD")
        .ok()
        .flatten()
        .map(|t| ref_namespace::strip_namespace_prefix(&t).into_owned());
    let head_oid = refs::resolve_ref(git_dir, "HEAD").ok();

    let symref_cap = match (&head_symref_logical, head_oid) {
        (Some(target), Some(_)) => format!(" symref=HEAD:{target}"),
        _ => String::new(),
    };

    let mut caps = format!(
        "multi_ack thin-pack side-band side-band-64k ofs-delta shallow deepen-since deepen-not \
         deepen-relative no-progress include-tag multi_ack_detailed allow-tip-sha1-in-want \
         allow-reachable-sha1-in-want no-done{symref_cap} filter object-format={object_format} \
         agent=git/{version} ref-in-want",
    );
    if trace2_transfer::transfer_advertise_sid_enabled(git_dir) {
        let sid = trace2_transfer::trace2_session_id_wire_once();
        caps.push_str(" session-id=");
        caps.push_str(&sid);
    }

    let mut first = true;
    let emit = |w: &mut dyn Write, oid: &ObjectId, name: &str, first: &mut bool| -> Result<()> {
        let line = if *first {
            *first = false;
            format!("{} {name}\0{caps}\n", oid.to_hex())
        } else {
            format!("{} {name}\n", oid.to_hex())
        };
        let len = 4 + line.len();
        write!(w, "{:04x}{}", len, line)?;
        Ok(())
    };

    // HEAD (namespace-resolved). `head_ref_namespaced` advertises HEAD only when it resolves; it is
    // never subject to hideRefs filtering (the HEAD pseudo-ref is always offered when present).
    if let Some(oid) = head_oid {
        emit(w, &oid, "HEAD", &mut first)?;
    }

    // All refs under the active namespace, advertised under their logical (stripped) names.
    // `refs::list_refs` already maps `GIT_NAMESPACE` onto `refs/namespaces/<ns>/...` and returns the
    // logical names, so we filter only with hideRefs here.
    let all_refs = refs::list_refs(git_dir, "refs/")?;
    for (refname, oid) in &all_refs {
        let full = ref_namespace::storage_ref_name(refname);
        if grit_lib::hide_refs::ref_is_hidden(refname, &full, &hide) {
            continue;
        }
        emit(w, oid, refname, &mut first)?;
        // Annotated tags advertise the peeled target as `<name>^{}`.
        if refname.starts_with("refs/tags/") {
            if let Some(peeled) = peel_to_target(git_dir, oid) {
                let line = format!("{} {refname}^{{}}\n", peeled.to_hex());
                let len = 4 + line.len();
                write!(w, "{:04x}{}", len, line)?;
            }
        }
    }

    // Nothing advertised yet (no namespaced HEAD and no refs under the namespace). When a namespace
    // is active, an empty namespace advertises only the `capabilities^{}` carrier (Git's
    // `send_ref`/no-ref path) so `ls-remote` reports an empty result (t5509 garbage namespace).
    // Without an active namespace, fall back to the unborn/detached HEAD advertisement so a
    // hash-aware client can still detect the object format (t5551 empty SHA-256 clone, proto v0).
    if first {
        if ref_namespace::ref_storage_prefix().is_some() {
            let z = zero_oid_hex_for_format(&object_format);
            let line = format!("{z} capabilities^{{}}\0{caps}\n");
            let len = 4 + line.len();
            write!(w, "{:04x}{}", len, line)?;
        } else {
            match resolve_head(git_dir) {
                Ok(HeadState::Detached { oid }) | Ok(HeadState::Branch { oid: Some(oid), .. }) => {
                    let line = format!("{} HEAD\0{caps}\n", oid.to_hex());
                    let len = 4 + line.len();
                    write!(w, "{:04x}{}", len, line)?;
                }
                Ok(HeadState::Branch { oid: None, .. }) => {
                    // Empty repository (unborn HEAD, no refs). Git advertises the no-ref carrier
                    // line `<zero-oid> capabilities^{}` here, NOT `<zero-oid> HEAD`: a client that
                    // sees a `HEAD` ref pointing at the all-zero OID would try to `want` it and the
                    // negotiation would fail, whereas `capabilities^{}` tells the client there are
                    // no refs to fetch while still carrying the capability list (incl.
                    // `object-format`) so a hash-aware client records the object format
                    // (t5551 "clone empty SHA-256 repository", proto v0).
                    let z = zero_oid_hex_for_format(&object_format);
                    let line = format!("{z} capabilities^{{}}\0{caps}\n");
                    let len = 4 + line.len();
                    write!(w, "{:04x}{}", len, line)?;
                }
                _ => {}
            }
        }
    }

    Ok(())
}

/// Peel an annotated tag to its ultimate non-tag target. Returns `None` for lightweight tags
/// (refs pointing directly at a commit/tree/blob).
fn peel_to_target(git_dir: &Path, oid: &ObjectId) -> Option<ObjectId> {
    let repo = Repository::open(git_dir, None).ok()?;
    let mut cur = *oid;
    let mut peeled_any = false;
    loop {
        let obj = repo.odb.read(&cur).ok()?;
        if obj.kind != ObjectKind::Tag {
            return if peeled_any { Some(cur) } else { None };
        }
        let tag = grit_lib::objects::parse_tag(&obj.data).ok()?;
        cur = tag.object;
        peeled_any = true;
    }
}

fn advertise_refs_with_caps(repo: &Repository, server_proto: u8) -> Result<()> {
    let mut out = io::stdout();
    if server_proto == 1 {
        pkt_line::write_line(&mut out, "version 1")?;
        out.flush()?;
    }
    write_ref_advertisement(&mut out, &repo.git_dir)?;
    write!(out, "0000")?;
    out.flush()?;
    Ok(())
}

/// Open a repository (bare or non-bare).
fn open_repo(path: &Path) -> Result<Repository> {
    if path.is_file() {
        let work_tree = path.parent().map(std::path::Path::to_path_buf);
        let git_dir = grit_lib::repo::resolve_dot_git(path)?;
        return Repository::open(&git_dir, work_tree.as_deref()).map_err(Into::into);
    }
    if let Ok(repo) = Repository::open(path, None) {
        return Ok(repo);
    }
    let dot_git = path.join(".git");
    if dot_git.is_file() {
        let resolved = grit_lib::repo::resolve_dot_git(&dot_git)?;
        return Repository::open(&resolved, Some(path)).map_err(Into::into);
    }
    Repository::open(&dot_git, Some(path)).map_err(Into::into)
}
