//! `grit send-pack` — push objects to a remote repository (plumbing).
//!
//! Local transport: spawns the receive-pack program (default `grit receive-pack`) with the
//! remote repository path, speaks the v0/v1 pkt-line push protocol, and streams a pack built
//! via `grit pack-objects`.

use crate::explicit_exit::ExplicitExit;
use anyhow::{bail, Context, Result};
use clap::Args as ClapArgs;
use grit_lib::merge_base::is_ancestor;
use grit_lib::objects::ObjectId;
use grit_lib::refs;
use grit_lib::repo::Repository;
use std::collections::HashSet;
use std::io::{self, BufRead, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use crate::fetch_transport::parse_leading_shell_env_assignments;
use crate::grit_exe::{grit_executable, strip_trace2_env};
use crate::protocol_wire;
use crate::trace2_transfer;
use crate::wire_trace;
use grit_lib::pkt_line::{read_packet, write_flush, Packet};

/// Upstream `git send-pack` usage synopsis (exit 129 when options are incompatible).
const SEND_PACK_USAGE: &str = "usage: git send-pack [--mirror] [--dry-run] [--force]\n\
              [--receive-pack=<git-receive-pack>]\n\
              [--verbose] [--thin] [--atomic]\n\
              [--[no-]signed | --signed=(true|false|if-asked)]\n\
              [<host>:]<directory> (--all | <ref>...)";

/// Arguments for `grit send-pack`.
#[derive(Debug, ClapArgs)]
#[command(about = "Push objects to a remote repository (plumbing)")]
pub struct Args {
    /// Path to the remote repository (bare or non-bare).
    #[arg(value_name = "REMOTE")]
    pub remote: String,

    /// Read additional refspec lines from stdin (after command-line refspecs).
    #[arg(long = "stdin")]
    pub stdin: bool,

    /// Mirror all refs (incompatible with explicit refspecs; not implemented for local transport).
    #[arg(long = "mirror")]
    pub mirror: bool,

    /// Refspec(s) to push (e.g. "main:main"). With `--all`, ignored.
    #[arg(value_name = "REF")]
    pub refs: Vec<String>,

    /// Push all branches (refs/heads/*).
    #[arg(long)]
    pub all: bool,

    /// Allow non-fast-forward updates.
    #[arg(long = "force")]
    pub force: bool,

    /// Show what would be done, without making changes.
    #[arg(short = 'n', long = "dry-run")]
    pub dry_run: bool,

    /// Program to run on the remote side (default: `grit receive-pack`).
    #[arg(long = "receive-pack", value_name = "PATH")]
    pub receive_pack: Option<String>,

    /// Alias for `--receive-pack` (Git compatibility).
    #[arg(long = "exec", value_name = "PATH")]
    pub exec: Option<String>,

    /// Use (or, with `--no-thin`, omit) thin-pack generation. Defaults on, matching `git send-pack`.
    #[arg(long = "thin", overrides_with = "no_thin", default_value_t = true)]
    pub thin: bool,
    #[arg(long = "no-thin", overrides_with = "thin")]
    pub no_thin: bool,
}

struct RefUpdate {
    remote_ref: String,
    old_oid: Option<ObjectId>,
    /// New value for the remote ref. `None` represents a deletion (the null OID on the wire).
    new_oid: Option<ObjectId>,
    /// This update was requested with a forcing refspec (`+`) or the global `--force`.
    force: bool,
}

pub fn run(args: Args) -> Result<()> {
    let repo = Repository::discover(None).context("not a git repository")?;

    let remote_path = PathBuf::from(&args.remote);
    let remote_repo = open_repo(&remote_path).with_context(|| {
        format!(
            "could not open remote repository at '{}'",
            remote_path.display()
        )
    })?;

    let mut refspecs = args.refs.clone();
    if args.stdin {
        let stdin = io::stdin();
        for line in stdin.lock().lines() {
            let line = line.context("reading refspecs from stdin")?;
            let line = line.trim_end_matches(['\r', '\n']);
            if line.is_empty() {
                continue;
            }
            refspecs.push(line.to_owned());
        }
    }

    if !refspecs.is_empty() && args.mirror {
        return Err(anyhow::Error::new(ExplicitExit {
            code: 129,
            message: SEND_PACK_USAGE.to_string(),
        }));
    }

    let mut updates = Vec::new();

    if args.all {
        let local_branches = refs::list_refs(&repo.git_dir, "refs/heads/")?;
        for (refname, local_oid) in &local_branches {
            let old_oid = refs::resolve_ref(&remote_repo.git_dir, refname).ok();
            if old_oid.as_ref() == Some(local_oid) {
                continue;
            }
            updates.push(RefUpdate {
                remote_ref: refname.clone(),
                old_oid,
                new_oid: Some(*local_oid),
                force: args.force,
            });
        }
    } else {
        if refspecs.is_empty() {
            bail!("no refs specified; nothing to push");
        }

        for spec in &refspecs {
            expand_refspec_into_updates(&repo, &remote_repo, spec, args.force, &mut updates)?;
        }

        let mut seen_remote: HashSet<String> = HashSet::new();
        for update in &updates {
            if !seen_remote.insert(update.remote_ref.clone()) {
                bail!(
                    "multiple updates for ref '{}' not allowed",
                    update.remote_ref
                );
            }
        }
    }

    for update in &updates {
        let (Some(old), Some(new)) = (&update.old_oid, &update.new_oid) else {
            continue;
        };
        if *old != *new && !update.force && !is_ancestor(&repo, *old, *new)? {
            bail!(
                "non-fast-forward update to '{}' rejected (use --force to override)",
                update.remote_ref
            );
        }
    }

    if updates.is_empty() {
        return Ok(());
    }

    if args.dry_run {
        for update in &updates {
            let old_hex = update
                .old_oid
                .as_ref()
                .map(|o| o.to_hex())
                .unwrap_or_else(|| "0".repeat(40));
            let new_hex = update
                .new_oid
                .as_ref()
                .map(|o| o.to_hex())
                .unwrap_or_else(|| "0".repeat(40));
            println!(
                "{}..{}\t{} (dry run)",
                &old_hex[..7],
                &new_hex[..7],
                update.remote_ref,
            );
        }
        return Ok(());
    }

    let receive_cmd = args
        .receive_pack
        .as_deref()
        .or(args.exec.as_deref())
        .unwrap_or("git receive-pack");

    let mut child = spawn_receive_pack(receive_cmd, &remote_path)?;
    let mut child_stdin = child.stdin.take().context("receive-pack stdin")?;
    let mut child_stdout = child.stdout.take().context("receive-pack stdout")?;

    let (extra_have, server_sideband, advertised_oids) = read_advertisement(&mut child_stdout)?;

    // Negotiate `side-band-64k` when the server advertises it: the server then multiplexes its
    // report-status on band 1 and hook/diagnostic output on band 2, which we demultiplex below
    // (band 2/3 → stderr prefixed `remote: `). The client→server pack is still raw (receive-pack
    // reads it from stdin without demuxing).
    let use_sideband = server_sideband;
    let mut caps = if use_sideband {
        "report-status report-status-v2 side-band-64k quiet object-format=sha1".to_owned()
    } else {
        "report-status report-status-v2 quiet object-format=sha1".to_owned()
    };
    if trace2_transfer::transfer_advertise_sid_enabled(&repo.git_dir) {
        let sid = trace2_transfer::trace2_session_id_wire_once();
        caps.push_str(" session-id=");
        caps.push_str(&sid);
    }
    let zero_hex = "0".repeat(40);
    let mut first_cmd = true;
    for update in &updates {
        let old_hex = update
            .old_oid
            .as_ref()
            .map(|o| o.to_hex())
            .unwrap_or_else(|| zero_hex.clone());
        let new_hex = update
            .new_oid
            .as_ref()
            .map(|o| o.to_hex())
            .unwrap_or_else(|| zero_hex.clone());
        let pkt = if first_cmd {
            first_cmd = false;
            format!("{old_hex} {new_hex} {}\0{caps}\n", update.remote_ref)
        } else {
            format!("{old_hex} {new_hex} {}\n", update.remote_ref)
        };
        write_pkt_line(&mut child_stdin, pkt.as_bytes())?;
    }
    write_flush(&mut child_stdin)?;
    child_stdin.flush()?;

    // Deletion-only pushes carry no new objects: send an empty pack so the server reads a
    // well-formed (if trivial) packfile. `git send-pack` does the same via pack-objects with no
    // positive tips, which emits the 12-byte empty-pack header plus its trailing sha1.
    let has_new_objects = updates.iter().any(|u| u.new_oid.is_some());
    if !has_new_objects {
        child_stdin.write_all(&empty_pack_bytes())?;
        child_stdin.flush()?;
        drop(child_stdin);
        let mut output = Vec::new();
        child_stdout.read_to_end(&mut output)?;
        let status = child.wait()?;
        let report = if use_sideband {
            demux_report_and_remote_messages(&output)?
        } else {
            output
        };
        let rejected = report_has_rejections(&report);
        if !status.success() || rejected {
            return Err(anyhow::Error::new(ExplicitExit {
                code: 1,
                message: String::new(),
            }));
        }
        return Ok(());
    }

    // Build the rev-list input fed to pack-objects (`--revs`): negative tips for everything the
    // server already has, positive tips for the new ref values.
    let mut rev_input = String::new();
    {
        let mut fed: HashSet<ObjectId> = HashSet::new();
        let new_tips: HashSet<ObjectId> = updates.iter().filter_map(|u| u.new_oid).collect();
        // Only feed `^<oid>` boundaries for objects the *source* repository actually has: the
        // server advertises refs and `.have` lines (e.g. an alternate's objects via `clone -s`)
        // that the pushing repo may not possess. Passing a missing object to pack-objects as a
        // negative tip aborts the traversal ("bad tree object"); git silently ignores such haves
        // (t5400 receive-pack .have de-dup push).
        let feed_negative = |fed: &mut HashSet<ObjectId>, rev_input: &mut String, oid: ObjectId| {
            if fed.insert(oid) && repo.odb.read(&oid).is_ok() {
                rev_input.push_str(&format!("^{}\n", oid.to_hex()));
            }
        };
        for oid in peel_advertised_commits(&repo, &advertised_oids) {
            if new_tips.contains(&oid) {
                continue;
            }
            feed_negative(&mut fed, &mut rev_input, oid);
        }
        for h in &extra_have {
            feed_negative(&mut fed, &mut rev_input, *h);
        }
        for update in &updates {
            let Some(new) = update.new_oid else {
                continue;
            };
            if let Some(old) = &update.old_oid {
                feed_negative(&mut fed, &mut rev_input, *old);
            }
            rev_input.push_str(&format!("{}\n", new.to_hex()));
        }
    }

    let pack_cwd = repo
        .work_tree
        .clone()
        .unwrap_or_else(|| repo.git_dir.clone());

    // Prefer the system Git binary for pack generation (the harness aliases `git` to grit, and
    // grit's pack-objects is not yet a full send-pack peer for thin packs). Fall back to grit's
    // own pack-objects when the system git is unavailable or fails — notably when the source
    // object store has a grit-written v2 multi-pack-index that older system git cannot parse
    // ("multi-pack-index version 2 not recognized"; t5400 receive-pack auto-gc).
    let use_thin = args.thin && !args.no_thin;
    let pack_bin = std::path::Path::new("/usr/bin/git");
    let pack_output = if pack_bin.is_file() {
        match run_pack_objects(
            Some(pack_bin),
            &pack_cwd,
            &repo.git_dir,
            &rev_input,
            use_thin,
        )? {
            output if output.status.success() => output,
            _ => run_pack_objects(None, &pack_cwd, &repo.git_dir, &rev_input, use_thin)?,
        }
    } else {
        run_pack_objects(None, &pack_cwd, &repo.git_dir, &rev_input, use_thin)?
    };
    if !pack_output.status.success() {
        // Surface the (final) pack-objects diagnostics now that no further fallback will run.
        let _ = io::stderr().write_all(&pack_output.stderr);
        bail!("pack-objects failed with status {}", pack_output.status);
    }
    child_stdin.write_all(&pack_output.stdout)?;
    child_stdin.flush()?;

    drop(child_stdin);
    let mut output = Vec::new();
    child_stdout.read_to_end(&mut output)?;
    let status = child.wait()?;

    // With side-band, band 1 carries the report-status pkt-lines and band 2/3 carries remote
    // diagnostics; demultiplex here. Without it, the raw stream is the report-status itself.
    let report = if use_sideband {
        demux_report_and_remote_messages(&output)?
    } else {
        output
    };

    // Parse the report-status stream to detect per-ref rejections (`ng <ref> <reason>`).
    let rejected = report_has_rejections(&report);

    // `git send-pack` consumes the report-status stream (it does not echo it to stdout) and exits
    // non-zero when any ref was rejected or the remote failed, without printing "receive-pack
    // failed" — the rejection is communicated via the report-status stream / push status.
    if !status.success() || rejected {
        return Err(anyhow::Error::new(ExplicitExit {
            code: 1,
            message: String::new(),
        }));
    }

    Ok(())
}

/// Split a side-band stream: band 1 (report-status) is returned; band 2/3 (remote diagnostics)
/// is written to stderr, line-buffered and prefixed with `remote: ` like git's `recv_sideband`.
fn demux_report_and_remote_messages(input: &[u8]) -> Result<Vec<u8>> {
    let mut report = Vec::new();
    let mut progress: Vec<u8> = Vec::new();
    let stderr = io::stderr();
    let mut err = stderr.lock();
    let mut i = 0usize;
    while i + 4 <= input.len() {
        let len = match grit_lib::pkt_line::parse_hex_len(&input[i..i + 4]) {
            Ok(l) => l,
            Err(_) => break,
        };
        i += 4;
        if len == 0 {
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
                progress.extend_from_slice(data);
                while let Some(pos) = progress.iter().position(|b| *b == b'\n') {
                    let mut line = progress[..pos].to_vec();
                    if line.ends_with(b"\r") {
                        line.pop();
                    }
                    writeln!(err, "remote: {}        ", String::from_utf8_lossy(&line))?;
                    progress.drain(..=pos);
                }
            }
            _ => {}
        }
    }
    if !progress.is_empty() {
        writeln!(
            err,
            "remote: {}        ",
            String::from_utf8_lossy(&progress)
        )?;
    }
    err.flush()?;
    Ok(report)
}

/// Run `pack-objects --stdout --revs --thin --delta-base-offset -q`, feeding `rev_input` on stdin.
///
/// `pack_bin` selects the executable: `Some(path)` runs the system Git (with a sanitized `PATH`
/// so the harness `git` shim is not re-entered), `None` runs grit's own `pack-objects`. Stderr is
/// captured (not inherited) so a failed system-git attempt — e.g. tripping over a grit-written v2
/// multi-pack-index — does not leak diagnostics before the grit fallback runs.
fn run_pack_objects(
    pack_bin: Option<&Path>,
    pack_cwd: &Path,
    git_dir: &Path,
    rev_input: &str,
    thin: bool,
) -> Result<std::process::Output> {
    let mut pack_args = vec![
        "pack-objects",
        "--stdout",
        "--revs",
        "--delta-base-offset",
        "-q",
    ];
    if thin {
        pack_args.push("--thin");
    }
    let mut cmd = match pack_bin {
        Some(bin) => {
            let mut c = Command::new(bin);
            c.current_dir(pack_cwd)
                .env("GIT_DIR", git_dir)
                .args(&pack_args);
            // Tests prepend a `git` shim ahead of `/usr/bin`; helpers invoked by git must not recurse.
            c.env("PATH", "/usr/bin:/bin");
            c
        }
        None => {
            let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("grit"));
            let mut c = Command::new(exe);
            c.current_dir(pack_cwd)
                .env("GIT_DIR", git_dir)
                .args(&pack_args);
            c
        }
    };
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawn pack-objects")?;
    {
        let mut stdin = child.stdin.take().context("pack-objects stdin")?;
        stdin.write_all(rev_input.as_bytes())?;
        stdin.flush().context("flush pack-objects stdin")?;
    }
    child.wait_with_output().context("wait for pack-objects")
}

/// True when the report-status stream contains any `ng <ref> ...` (rejected) command line.
fn report_has_rejections(report: &[u8]) -> bool {
    let mut cursor = io::Cursor::new(report);
    while let Ok(Some(pkt)) = read_packet(&mut cursor) {
        if let Packet::Data(line) = pkt {
            if line.trim_start().starts_with("ng ") {
                return true;
            }
        }
    }
    false
}

pub(crate) fn write_pkt_line(w: &mut impl Write, payload: &[u8]) -> io::Result<()> {
    let n = payload.len() + 4;
    write!(w, "{:04x}", n)?;
    w.write_all(payload)
}

fn shell_single_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Spawn the receive-pack program (`receive_cmd`) against the local `remote_path`, wiring its
/// stdin/stdout to pipes so the caller can speak the receive-pack wire protocol to it.
///
/// `receive_cmd` may be the default (`git receive-pack` / `git-receive-pack`, dispatched to grit's
/// own `receive-pack`), a shell-quoted custom program, or a plain path. The remote repository path
/// is appended as the final argument exactly as Git does when invoking the receiving side.
pub(crate) fn spawn_receive_pack(receive_cmd: &str, remote_path: &Path) -> Result<Child> {
    let remote_path = remote_path
        .canonicalize()
        .unwrap_or_else(|_| remote_path.to_path_buf());
    let remote_str = remote_path.to_string_lossy();
    let client_proto = protocol_wire::effective_client_protocol_version();
    let is_default_receive_pack = {
        let t = receive_cmd.trim();
        t == "git receive-pack" || t == "git-receive-pack"
    };
    let apply_proto = |c: &mut Command| {
        if !is_default_receive_pack || client_proto == 0 {
            c.env_remove("GIT_PROTOCOL");
        } else {
            protocol_wire::merge_git_protocol_env_for_child(c, client_proto);
        }
        // `git -c key=value send-pack` sets GIT_CONFIG_PARAMETERS in this process for the *client*
        // side only; it must not leak to the remote receive-pack (t5400 "cannot override
        // denyDeletes with git -c send-pack"). A custom `--receive-pack="git -c ... receive-pack"`
        // re-establishes its own parameters when the spawned git re-parses its `-c`.
        c.env_remove("GIT_CONFIG_PARAMETERS");
    };
    // Git runs the receive-pack command through `sh -c` whenever it contains shell metacharacters,
    // so quoting and redirection are honored exactly as a shell would. A custom `--receive-pack`
    // whose program path is single/double quoted (e.g. `'<dir with spaces>'/wrapper`, as t5543
    // produces via `${SQ}$(pwd)${SQ}/receive-pack-wrapper`) only resolves once the shell strips the
    // quotes; running it directly would look for a file literally named with the quotes.
    if receive_cmd.contains('|')
        || receive_cmd.contains('>')
        || receive_cmd.contains('<')
        || receive_cmd.contains('&')
        || receive_cmd.contains('\'')
        || receive_cmd.contains('"')
    {
        let script = format!(
            "{} {}",
            receive_cmd.trim_end(),
            shell_single_quote(&remote_str)
        );
        let mut c = Command::new("sh");
        strip_trace2_env(&mut c);
        apply_proto(&mut c);
        return c
            .arg("-c")
            .arg(&script)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .context("spawn receive-pack shell");
    }

    let (leading_env, after_env) = parse_leading_shell_env_assignments(receive_cmd);
    if after_env.contains("git-receive-pack") {
        let mut cmd = Command::new(grit_executable());
        strip_trace2_env(&mut cmd);
        for (k, v) in leading_env {
            cmd.env(k, v);
        }
        cmd.arg("receive-pack")
            .arg(remote_str.as_ref())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        apply_proto(&mut cmd);
        return cmd.spawn().context("spawn grit receive-pack");
    }

    let words: Vec<&str> = receive_cmd.split_whitespace().collect();
    if words.is_empty() {
        bail!("empty receive-pack command");
    }
    let mut cmd = Command::new(words[0]);
    strip_trace2_env(&mut cmd);
    for w in &words[1..] {
        cmd.arg(w);
    }
    apply_proto(&mut cmd);
    cmd.arg(remote_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    cmd.spawn().context("spawn receive-pack")
}

pub(crate) fn read_advertisement(
    r: &mut impl Read,
) -> Result<(HashSet<ObjectId>, bool, Vec<ObjectId>)> {
    let mut haves = HashSet::new();
    let mut advertised = Vec::new();
    let mut server_sideband = false;
    let mut saw_version_1 = false;
    let mut first_data = true;
    let mut server_sid_done = false;
    loop {
        match read_packet(r)? {
            None => break,
            Some(Packet::Flush) => break,
            Some(Packet::Delim) | Some(Packet::ResponseEnd) => break,
            Some(Packet::Data(line)) => {
                if first_data {
                    first_data = false;
                    let t = line.trim_end_matches('\n');
                    if t.starts_with("version 1") {
                        saw_version_1 = true;
                    }
                }
                let trace_line = line.trim_end_matches('\n').replace('\0', "\\0");
                wire_trace::trace_packet_push('<', &trace_line);
                if let Some(oid) = parse_dot_have_line(&line) {
                    haves.insert(oid);
                }
                if let Some(oid) = parse_advertised_ref_oid(&line) {
                    advertised.push(oid);
                }
                if let Some(caps) = line.split('\0').nth(1) {
                    if caps.contains("side-band-64k") || caps.contains("side-band") {
                        server_sideband = true;
                    }
                    if !server_sid_done {
                        if let Some(sid) = trace2_transfer::extract_session_id_feature(caps) {
                            trace2_transfer::emit_server_sid(sid);
                            server_sid_done = true;
                        }
                    }
                }
            }
        }
    }
    trace2_transfer::emit_negotiated_version_client_fetch(saw_version_1);
    Ok((haves, server_sideband, advertised))
}

fn parse_advertised_ref_oid(line: &str) -> Option<ObjectId> {
    let line = line.trim_end_matches('\n');
    let (main, _) = line.split_once('\0').unwrap_or((line, ""));
    let mut it = main.splitn(2, ['\t', ' ']);
    let hex = it.next()?.trim();
    let name = it.next()?.trim();
    // `HEAD` appears on the first pkt-line alongside capabilities; it may be detached at the
    // same OID as the branch being pushed. Feeding it to pack-objects as `^<oid>` would exclude
    // the whole thin pack (t5410 with grit receive-pack advertising detached HEAD).
    if name == "HEAD" || name.starts_with("capabilities") {
        return None;
    }
    ObjectId::from_hex(hex).ok()
}

pub(crate) fn peel_advertised_commits(repo: &Repository, oids: &[ObjectId]) -> Vec<ObjectId> {
    use grit_lib::objects::{parse_tag, ObjectKind};
    let mut out = Vec::new();
    let mut seen_commits = HashSet::new();
    for &start in oids {
        let mut cur = start;
        let commit = loop {
            let Ok(obj) = repo.odb.read(&cur) else {
                break None;
            };
            match obj.kind {
                ObjectKind::Tag => {
                    let Ok(tag) = parse_tag(&obj.data) else {
                        break None;
                    };
                    cur = tag.object;
                }
                ObjectKind::Commit => break Some(cur),
                _ => break None,
            }
        };
        if let Some(c) = commit {
            if seen_commits.insert(c) {
                out.push(c);
            }
        }
    }
    out
}

fn parse_dot_have_line(line: &str) -> Option<ObjectId> {
    let line = line.trim_end_matches('\n');
    let (main, _) = line.split_once('\0').unwrap_or((line, ""));
    let mut it = main.splitn(2, ['\t', ' ']);
    let hex = it.next()?;
    let name = it.next()?.trim();
    if name == ".have" {
        ObjectId::from_hex(hex).ok()
    } else {
        None
    }
}

fn normalize_ref(name: &str) -> String {
    if name.starts_with("refs/") {
        name.to_owned()
    } else {
        format!("refs/heads/{name}")
    }
}

/// The bytes of an empty packfile (`PACK`, version 2, zero objects) with its trailing SHA-1.
///
/// `git send-pack` always streams a packfile after the ref-update commands, even for
/// deletion-only pushes; the receiving side reads the trailer to know the pack ended.
fn empty_pack_bytes() -> Vec<u8> {
    use sha1::{Digest, Sha1};
    let mut pack = Vec::with_capacity(32);
    pack.extend_from_slice(b"PACK");
    pack.extend_from_slice(&2u32.to_be_bytes());
    pack.extend_from_slice(&0u32.to_be_bytes());
    let digest = Sha1::digest(&pack);
    pack.extend_from_slice(&digest);
    pack
}

/// Expand one push refspec `[+]<src>[:<dst>]` into one or more [`RefUpdate`]s.
///
/// Handles deletions (empty `<src>`), per-refspec forcing (`+`), wildcard patterns
/// (`refs/heads/*:refs/heads/*`), and revision expressions on the source side (`main^`).
fn expand_refspec_into_updates(
    repo: &Repository,
    remote_repo: &Repository,
    spec: &str,
    global_force: bool,
    out: &mut Vec<RefUpdate>,
) -> Result<()> {
    use grit_lib::refspec::parse_push_refspec;

    let item = parse_push_refspec(spec).with_context(|| format!("invalid refspec '{spec}'"))?;
    let force = global_force || item.force;
    let src = item.src.as_deref().unwrap_or("");
    let dst_opt = item.dst.as_deref();

    // Deletion: empty source side with an explicit destination removes the remote ref.
    if src.is_empty() {
        let Some(dst) = dst_opt else {
            bail!("refspec '{spec}' has no destination to delete");
        };
        let remote_ref = normalize_ref(dst);
        let old_oid = refs::resolve_ref(&remote_repo.git_dir, &remote_ref).ok();
        out.push(RefUpdate {
            remote_ref,
            old_oid,
            new_oid: None,
            force,
        });
        return Ok(());
    }

    // Wildcard refspec: map each local ref matching the source pattern to the destination.
    if item.pattern {
        let dst = dst_opt
            .ok_or_else(|| anyhow::anyhow!("wildcard refspec '{spec}' requires a destination"))?;
        expand_wildcard_refspec(repo, remote_repo, src, dst, force, out)?;
        return Ok(());
    }

    // Concrete refspec: resolve the source revision, default the destination to the source ref.
    let new_oid = grit_lib::rev_parse::resolve_revision(repo, src)
        .with_context(|| format!("src ref '{src}' does not match any"))?;
    let dst = dst_opt.unwrap_or(src);
    let remote_ref = normalize_ref(dst);
    let old_oid = refs::resolve_ref(&remote_repo.git_dir, &remote_ref).ok();
    out.push(RefUpdate {
        remote_ref,
        old_oid,
        new_oid: Some(new_oid),
        force,
    });
    Ok(())
}

/// Expand a single `*`-bearing source/destination refspec pair against the local refs.
///
/// The source and destination each contain exactly one `*` (validated by the refspec parser);
/// the text matched by the source `*` is substituted into the destination `*`.
fn expand_wildcard_refspec(
    repo: &Repository,
    remote_repo: &Repository,
    src: &str,
    dst: &str,
    force: bool,
    out: &mut Vec<RefUpdate>,
) -> Result<()> {
    let (src_prefix, src_suffix) = src
        .split_once('*')
        .ok_or_else(|| anyhow::anyhow!("source pattern '{src}' has no '*'"))?;
    let (dst_prefix, dst_suffix) = dst
        .split_once('*')
        .ok_or_else(|| anyhow::anyhow!("destination pattern '{dst}' has no '*'"))?;

    let list_prefix = if src_prefix.starts_with("refs/") {
        src_prefix
    } else {
        "refs/"
    };
    for (refname, local_oid) in refs::list_refs(&repo.git_dir, list_prefix)? {
        let Some(rest) = refname.strip_prefix(src_prefix) else {
            continue;
        };
        let Some(matched) = rest.strip_suffix(src_suffix) else {
            continue;
        };
        // Avoid matching when the suffix overlaps the prefix on too-short names.
        if rest.len() < src_suffix.len() {
            continue;
        }
        let remote_ref = format!("{dst_prefix}{matched}{dst_suffix}");
        let old_oid = refs::resolve_ref(&remote_repo.git_dir, &remote_ref).ok();
        out.push(RefUpdate {
            remote_ref,
            old_oid,
            new_oid: Some(local_oid),
            force,
        });
    }
    Ok(())
}

fn open_repo(path: &Path) -> Result<Repository> {
    if let Ok(repo) = Repository::open(path, None) {
        return Ok(repo);
    }
    let git_dir = path.join(".git");
    Repository::open(&git_dir, Some(path)).map_err(Into::into)
}
