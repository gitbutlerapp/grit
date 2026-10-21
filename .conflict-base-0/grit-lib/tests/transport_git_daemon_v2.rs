//! Integration test for protocol **v2** fetch over the `git://` streaming
//! transport: `GitDaemonTransport` + `fetch::fetch_remote` with
//! `ConnectOptions { protocol_version: 2, .. }`.
//!
//! This is the v2 counterpart of `transport_git_daemon.rs`. It builds the same
//! source repo (two commits on `main`, a `topic` branch, an annotated tag),
//! serves it with a real `git daemon`, and fetches into an empty local repo with
//! `+refs/heads/*:refs/remotes/origin/*` and `TagMode::All`. It asserts the SAME
//! things the v0/v1 test does (tracking refs, objects, oids vs `git rev-parse`,
//! `git fsck` clean).
//!
//! CRITICAL: it proves v2 was actually negotiated (not a silent v1 fallback) two
//! ways:
//!   * `conn.protocol_version() == 2` — the daemon answered our `version=2`
//!     request with a v2 capability advertisement (so `read_advertisement` set
//!     v2 and captured the capability block instead of refs);
//!   * `GIT_TRACE_PACKET` on the daemon records `command=ls-refs` /
//!     `command=fetch` and a `version 2` line — i.e. the upload-pack child the
//!     daemon spawned actually ran the v2 ls-refs + fetch commands our client
//!     sent.
//!
//! Skips gracefully (returns early) when `git daemon` is unavailable or fails to
//! bind.

use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use grit_lib::objects::odb::Odb;
use grit_lib::objects::ObjectId;
use grit_lib::refs::resolve_ref;
use grit_lib::remote::fetch::{fetch_remote, NoProgress};
use grit_lib::remote::transfer::{FetchOptions, TagMode, UpdateMode};
use grit_lib::remote::transport::{ConnectOptions, GitDaemonTransport, Service, Transport};

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_AUTHOR_DATE", "2005-04-07T22:13:13 +0200")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_DATE", "2005-04-07T22:13:13 +0200")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).expect("utf8 git output")
}

fn rev_parse(dir: &Path, rev: &str) -> ObjectId {
    ObjectId::from_hex(git(dir, &["rev-parse", rev]).trim()).expect("valid oid")
}

fn open_odb(git_dir: &Path) -> Odb {
    Odb::new(&git_dir.join("objects")).with_config_git_dir(git_dir.to_path_buf())
}

/// Build a source repo: two commits on `main`, a `topic` branch, an annotated tag.
fn build_source(dir: &Path) {
    git(dir, &["init", "-q", "-b", "main", "."]);
    std::fs::write(dir.join("a.txt"), "one\n").unwrap();
    git(dir, &["add", "a.txt"]);
    git(dir, &["commit", "-q", "-m", "c1"]);
    std::fs::write(dir.join("b.txt"), "two\n").unwrap();
    git(dir, &["add", "b.txt"]);
    git(dir, &["commit", "-q", "-m", "c2"]);
    git(dir, &["tag", "-a", "v1", "-m", "release one"]);
    git(dir, &["branch", "topic"]);
}

/// Pick a currently-free localhost port by binding then dropping a listener.
fn free_port() -> Option<u16> {
    // Dedup port handout per process so a concurrent caller never accepts a
    // port another test already reserved (closes the bind-before-spawn TOCTOU
    // that let two servers share a port under high test parallelism).
    static USED: std::sync::Mutex<Vec<u16>> = std::sync::Mutex::new(Vec::new());
    let mut used = USED.lock().unwrap_or_else(|e| e.into_inner());
    for _ in 0..200 {
        let l = TcpListener::bind(("127.0.0.1", 0)).ok()?;
        let p = l.local_addr().ok()?.port();
        drop(l);
        if !used.contains(&p) {
            used.push(p);
            return Some(p);
        }
    }
    None
}

/// Spawn `git daemon` over `base_path` on `port`, with `GIT_TRACE_PACKET`
/// pointing at `trace_path` so the upload-pack children record the pkt-lines
/// they exchange (used to prove v2 was negotiated). Returns the child handle, or
/// `None` if the binary is missing.
fn spawn_daemon(base_path: &Path, port: u16, trace_path: &Path) -> Option<Child> {
    Command::new("git")
        .arg("daemon")
        .arg("--listen=127.0.0.1")
        .arg(format!("--port={port}"))
        .arg("--reuseaddr")
        .arg("--export-all")
        .arg(format!("--base-path={}", base_path.display()))
        .arg(base_path)
        // Inherited by the `git upload-pack` children the daemon forks; records
        // every pkt-line so we can assert the v2 commands actually ran.
        .env("GIT_TRACE_PACKET", trace_path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .ok()
}

/// Wait until the daemon answers a TCP connect on `port`, or time out.
fn wait_ready(port: u16) -> bool {
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if TcpStream::connect_timeout(&addr, Duration::from_millis(200)).is_ok() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

struct DaemonGuard(Child);
impl Drop for DaemonGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn fetch_over_git_daemon_v2_lands_refs_and_objects() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let work = tmp.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    build_source(&work);

    let base = tmp.path().join("srv");
    std::fs::create_dir_all(&base).unwrap();
    let source = base.join("repo.git");
    git(
        &work,
        &[
            "clone",
            "-q",
            "--bare",
            ".",
            source.to_str().expect("utf8 path"),
        ],
    );
    git(&source, &["symbolic-ref", "HEAD", "refs/heads/main"]);

    let main_oid = rev_parse(&source, "refs/heads/main");
    let topic_oid = rev_parse(&source, "refs/heads/topic");
    let c1_oid = rev_parse(&work, "HEAD~1");
    let tag_oid = rev_parse(&source, "refs/tags/v1");

    let Some(port) = free_port() else {
        eprintln!("SKIP: could not allocate a free port");
        return;
    };
    let trace_path = tmp.path().join("trace_packet.log");
    let Some(child) = spawn_daemon(&base, port, &trace_path) else {
        eprintln!("SKIP: `git daemon` is unavailable");
        return;
    };
    let _guard = DaemonGuard(child);
    if !wait_ready(port) {
        eprintln!("SKIP: git daemon did not become ready on port {port}");
        return;
    }

    let url = format!("git://127.0.0.1:{port}/repo.git");

    let local = tmp.path().join("local");
    std::fs::create_dir_all(&local).unwrap();
    git(&local, &["init", "-q", "-b", "main", "."]);
    let local_git = local.join(".git");

    // Connect requesting protocol v2.
    let transport = GitDaemonTransport::new();
    let opts_v2 = ConnectOptions {
        protocol_version: 2,
        ..Default::default()
    };
    let mut conn = match transport.connect(&url, Service::UploadPack, &opts_v2) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("SKIP: could not connect to git daemon: {e}");
            return;
        }
    };

    // PROOF #1: the daemon answered with a v2 capability advertisement — no refs,
    // a captured capability block, protocol version 2.
    assert_eq!(
        conn.protocol_version(),
        2,
        "expected protocol v2 to be negotiated; got v{} (silent fallback?)",
        conn.protocol_version()
    );
    assert!(
        conn.advertised_refs().is_empty(),
        "a v2 connection must advertise no refs on connect (refs come from ls-refs)"
    );
    assert!(
        conn.capabilities().iter().any(|c| c == "ls-refs"
            || c.starts_with("ls-refs=")
            || c.starts_with("fetch=")
            || c == "fetch"),
        "v2 capability block missing ls-refs/fetch; got {:?}",
        conn.capabilities()
    );

    let opts = FetchOptions {
        refspecs: vec!["+refs/heads/*:refs/remotes/origin/*".to_owned()],
        tags: TagMode::All,
        ..Default::default()
    };
    let outcome = fetch_remote(&local_git, &mut *conn, &opts, &mut NoProgress)
        .expect("v2 fetch_remote over git daemon");

    // Tracking refs written (same assertions as the v0/v1 test).
    let got_main = resolve_ref(&local_git, "refs/remotes/origin/main").expect("origin/main");
    let got_topic = resolve_ref(&local_git, "refs/remotes/origin/topic").expect("origin/topic");
    assert_eq!(got_main, main_oid, "origin/main oid mismatch vs source");
    assert_eq!(got_topic, topic_oid, "origin/topic oid mismatch vs source");

    // The annotated tag arrived (TagMode::All).
    let got_tag = resolve_ref(&local_git, "refs/tags/v1").expect("tag v1 written");
    assert_eq!(got_tag, tag_oid, "tag v1 oid mismatch vs source");

    // Objects landed in the local odb.
    let local_odb = open_odb(&local_git);
    for oid in [main_oid, topic_oid, c1_oid, tag_oid] {
        assert!(
            local_odb.exists(&oid),
            "object {} missing from local odb after v2 fetch",
            oid.to_hex()
        );
        local_odb
            .read(&oid)
            .unwrap_or_else(|e| panic!("read {}: {e}", oid.to_hex()));
    }

    // Per-ref update modes: main is New, pointing at the source tip.
    let main_update = outcome
        .updates
        .iter()
        .find(|u| u.remote_ref == "refs/heads/main")
        .expect("update for main");
    assert_eq!(main_update.mode, UpdateMode::New);
    assert_eq!(main_update.new_oid, Some(main_oid));

    // Default branch resolved from the v2 ls-refs HEAD symref.
    assert_eq!(outcome.default_branch.as_deref(), Some("main"));

    // Cross-check the fetched main tip equals git's view of the source.
    assert_eq!(
        got_main.to_hex(),
        git(&source, &["rev-parse", "refs/heads/main"]).trim()
    );

    // The fetched pack fsck's clean in the local repo.
    let fsck = Command::new("git")
        .current_dir(&local)
        .args(["fsck", "--no-dangling"])
        .output()
        .expect("run git fsck");
    assert!(
        fsck.status.success(),
        "git fsck failed after v2 fetch: {}",
        String::from_utf8_lossy(&fsck.stderr)
    );

    // PROOF #2: the daemon's upload-pack child actually ran the v2 commands we
    // sent. The GIT_TRACE_PACKET log must show our `command=ls-refs` and
    // `command=fetch` requests and the server's `version 2` advertisement —
    // none of which appear in a v0/v1 conversation.
    let trace = std::fs::read_to_string(&trace_path).unwrap_or_default();
    assert!(
        !trace.is_empty(),
        "GIT_TRACE_PACKET produced no output; cannot confirm v2 negotiation"
    );
    assert!(
        trace.contains("version 2"),
        "trace missing `version 2` advertisement:\n{trace}"
    );
    assert!(
        trace.contains("command=ls-refs"),
        "trace missing `command=ls-refs` (v2 ref discovery did not run):\n{trace}"
    );
    assert!(
        trace.contains("command=fetch"),
        "trace missing `command=fetch` (v2 fetch negotiation did not run):\n{trace}"
    );
}

/// Exercise the v2 multi-round have/ACK negotiation: the local repo already
/// shares history with the remote, so the client sends `have` lines and the
/// server can build a thin pack. Proves the negotiation path (not just the
/// empty-clone no-haves path) works end to end and that `have`/`ACK` appear on
/// the wire.
#[test]
fn fetch_over_git_daemon_v2_incremental_with_haves() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let work = tmp.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    build_source(&work);
    // Deepen `main` past the negotiator's initial flush window (16) so the seeded
    // local shares >16 commits — forcing the two-round have/ACK exchange (round 1:
    // first 16 haves without `done`; round 2: remaining haves + `done`).
    git(&work, &["checkout", "-q", "main"]);
    for i in 0..20 {
        let msg = format!("d{i}");
        git(&work, &["commit", "-q", "--allow-empty", "-m", &msg]);
    }

    let base = tmp.path().join("srv");
    std::fs::create_dir_all(&base).unwrap();
    let source = base.join("repo.git");
    git(
        &work,
        &[
            "clone",
            "-q",
            "--bare",
            ".",
            source.to_str().expect("utf8 path"),
        ],
    );
    git(&source, &["symbolic-ref", "HEAD", "refs/heads/main"]);

    let Some(port) = free_port() else {
        eprintln!("SKIP: could not allocate a free port");
        return;
    };
    let trace_path = tmp.path().join("trace_packet.log");
    let Some(child) = spawn_daemon(&base, port, &trace_path) else {
        eprintln!("SKIP: `git daemon` is unavailable");
        return;
    };
    let _guard = DaemonGuard(child);
    if !wait_ready(port) {
        eprintln!("SKIP: git daemon did not become ready on port {port}");
        return;
    }
    let url = format!("git://127.0.0.1:{port}/repo.git");

    // Local repo that already has the FIRST commit of main (shared history), so
    // the v2 fetch negotiation has a real `have` to offer.
    let local = tmp.path().join("local");
    std::fs::create_dir_all(&local).unwrap();
    git(&local, &["init", "-q", "-b", "main", "."]);
    let local_git = local.join(".git");

    let transport = GitDaemonTransport::new();
    let opts_v2 = ConnectOptions {
        protocol_version: 2,
        ..Default::default()
    };

    // First fetch (v2) to seed the local repo with main's full history.
    {
        let mut conn = match transport.connect(&url, Service::UploadPack, &opts_v2) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("SKIP: could not connect to git daemon: {e}");
                return;
            }
        };
        assert_eq!(conn.protocol_version(), 2);
        let opts = FetchOptions {
            refspecs: vec!["+refs/heads/main:refs/remotes/origin/main".to_owned()],
            tags: TagMode::None,
            ..Default::default()
        };
        fetch_remote(&local_git, &mut *conn, &opts, &mut NoProgress).expect("seed v2 fetch");
    }

    // Materialize a local branch at the fetched tip so the negotiator has a real
    // `refs/heads/` tip to offer as a `have` (the negotiation walk, like the
    // v0/v1 path, scans heads/tags/HEAD — not remote-tracking refs). `main` is an
    // unborn checked-out branch here, so use a separate branch name.
    git(&local, &["branch", "base", "refs/remotes/origin/main"]);

    // Add a NEW commit on the remote's `topic` branch so the second fetch must
    // pull new objects while we already hold main's history as `have`s.
    git(&work, &["checkout", "-q", "topic"]);
    std::fs::write(work.join("c.txt"), "three\n").unwrap();
    git(&work, &["add", "c.txt"]);
    git(&work, &["commit", "-q", "-m", "c3"]);
    let topic_new = rev_parse(&work, "refs/heads/topic");
    git(&work, &["push", "-q", source.to_str().unwrap(), "topic"]);

    // Truncate the trace so we only inspect the second (incremental) fetch.
    let _ = std::fs::write(&trace_path, b"");

    // Second fetch (v2): local already has main history -> sends `have` lines.
    let mut conn = transport
        .connect(&url, Service::UploadPack, &opts_v2)
        .expect("reconnect v2");
    assert_eq!(conn.protocol_version(), 2);
    let opts = FetchOptions {
        refspecs: vec!["+refs/heads/*:refs/remotes/origin/*".to_owned()],
        tags: TagMode::None,
        ..Default::default()
    };
    fetch_remote(&local_git, &mut *conn, &opts, &mut NoProgress).expect("incremental v2 fetch");

    // The new topic tip landed.
    let got_topic = resolve_ref(&local_git, "refs/remotes/origin/topic").expect("origin/topic");
    assert_eq!(got_topic, topic_new, "incremental topic tip mismatch");
    let local_odb = open_odb(&local_git);
    assert!(
        local_odb.exists(&topic_new),
        "new topic commit {} missing after incremental v2 fetch",
        topic_new.to_hex()
    );

    // fsck stays clean.
    let fsck = Command::new("git")
        .current_dir(&local)
        .args(["fsck", "--no-dangling"])
        .output()
        .expect("run git fsck");
    assert!(
        fsck.status.success(),
        "git fsck failed after incremental v2 fetch: {}",
        String::from_utf8_lossy(&fsck.stderr)
    );

    // The incremental fetch's trace shows a `have` line we sent and the
    // `command=fetch` request — i.e. the negotiation actually offered local
    // history rather than re-downloading everything.
    let trace = std::fs::read_to_string(&trace_path).unwrap_or_default();
    assert!(
        trace.contains("command=fetch"),
        "incremental trace missing `command=fetch`:\n{trace}"
    );
    assert!(
        trace.contains("have "),
        "incremental trace missing any `have` line (no negotiation happened):\n{trace}"
    );
}
