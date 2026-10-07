//! End-to-end transport matrix over a **SHA-256** (`object-format=sha256`) repo.
//!
//! Every other transport test in this crate exercises the default SHA-1 object
//! format. This file is the SHA-256 counterpart: it builds real sha256 fixtures
//! with the system `git` (`git init --object-format=sha256`) and drives the
//! grit-lib transport stack against them, asserting that:
//!
//!   * 64-hex (32-byte) object ids round-trip on the wire and land on disk;
//!   * `object-format=sha256` is genuinely negotiated in the capability
//!     advertisement (not a silent sha1 fallback);
//!   * the expected refs/objects arrive and resolve to the source oids;
//!   * `git fsck` is clean on the resulting repos (cross-check vs system git).
//!
//! Matrix:
//!   * fetch over `git://` (protocol **v2**)        — `GitDaemonTransport` + `fetch_remote`
//!   * fetch over smart-HTTP (v0/v1 stateless RPC)  — `SmartHttpTransport` + `http_fetch`
//!   * push  over `git://` (v0/v1 receive-pack)     — `GitDaemonTransport` + `push_remote`
//!   * push  over smart-HTTP (v0/v1 receive-pack)   — `push_http`
//!
//! The library reads the repo hash algo from `extensions.objectFormat`, so the
//! *local* repos here are also created with `--object-format=sha256`; a mismatch
//! would surface as a parse/width error rather than a silent pass.
//!
//! Fixtures are the same real ones the sibling tests use: a system `git daemon`
//! for `git://` and the `grit-http-server` binary (whose upload-pack/receive-pack
//! are grit-lib's own hash-algo-aware implementations) for HTTP. Each test SKIPs
//! cleanly (returns early) only when its fixture is genuinely unavailable — the
//! local git lacking sha256 support, no free port, the daemon/server failing to
//! bind, or the server binary missing. The happy path is otherwise a real
//! assertion against live wire I/O.
//!
//! Gated on `http-ureq` (the default `UreqHttpClient` lives there):
//!   cargo test -p grit-lib --features http-ureq --test matrix_sha256

#![cfg(feature = "http-ureq")]

use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use grit_lib::fetch::{fetch_remote, NoProgress};
use grit_lib::objects::{HashAlgo, ObjectId};
use grit_lib::odb::Odb;
use grit_lib::push::{push_http, push_remote};
use grit_lib::push_report::PushRefStatus;
use grit_lib::refs::resolve_ref;
use grit_lib::transfer::{FetchOptions, PushOptions, PushRefSpec, TagMode, UpdateMode};
use grit_lib::transport::http::ureq_client::UreqHttpClient;
use grit_lib::transport::http::{http_fetch, HttpClient, SmartHttpTransport};
use grit_lib::transport::{ConnectOptions, GitDaemonTransport, Service, Transport};

// ---------------------------------------------------------------------------
// Shared helpers (mirrors the sibling transport_*.rs harnesses).
// ---------------------------------------------------------------------------

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

/// Run `git` like [`git`] but return the (success, stdout, stderr) triple instead
/// of asserting — used for capability probes that are allowed to fail (skip).
fn git_try(dir: &Path, args: &[&str]) -> (bool, String, String) {
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
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn rev_parse(dir: &Path, rev: &str) -> ObjectId {
    ObjectId::from_hex(git(dir, &["rev-parse", rev]).trim()).expect("valid oid")
}

fn open_odb(git_dir: &Path) -> Odb {
    Odb::new(&git_dir.join("objects")).with_config_git_dir(git_dir.to_path_buf())
}

/// True if the local `git` can create sha256 repos. We probe once at the start of
/// each test and SKIP cleanly when it cannot (older git builds).
fn git_supports_sha256() -> bool {
    let tmp = match tempfile::tempdir() {
        Ok(t) => t,
        Err(_) => return false,
    };
    let (ok, _, _) = git_try(
        tmp.path(),
        &["init", "-q", "--object-format=sha256", "probe"],
    );
    if !ok {
        return false;
    }
    let probe = tmp.path().join("probe");
    let (ok, out, _) = git_try(&probe, &["rev-parse", "--show-object-format"]);
    ok && out.trim() == "sha256"
}

/// Build a sha256 source repo: two commits on `main`, a `topic` branch, an
/// annotated tag. Returns nothing; the caller bare-clones it.
fn build_sha256_source(dir: &Path) {
    git(
        dir,
        &["init", "-q", "-b", "main", "--object-format=sha256", "."],
    );
    // Cross-check: this really is a sha256 repo.
    assert_eq!(
        git(dir, &["rev-parse", "--show-object-format"]).trim(),
        "sha256",
        "fixture repo was not created as sha256"
    );
    std::fs::write(dir.join("a.txt"), "one\n").unwrap();
    git(dir, &["add", "a.txt"]);
    git(dir, &["commit", "-q", "-m", "c1"]);
    std::fs::write(dir.join("b.txt"), "two\n").unwrap();
    git(dir, &["add", "b.txt"]);
    git(dir, &["commit", "-q", "-m", "c2"]);
    git(dir, &["tag", "-a", "v1", "-m", "release one"]);
    git(dir, &["branch", "topic"]);
}

/// Assert an oid is sha256-shaped: 64 hex chars / 32 bytes.
fn assert_sha256_oid(oid: &ObjectId, what: &str) {
    let hex = oid.to_hex();
    assert_eq!(
        hex.len(),
        HashAlgo::Sha256.hex_len(),
        "{what} oid {hex} is not 64-hex (sha256); got {} chars",
        hex.len()
    );
    assert!(
        hex.chars().all(|c| c.is_ascii_hexdigit()),
        "{what} oid {hex} contains non-hex characters"
    );
}

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

fn wait_ready(port: u16) -> bool {
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if TcpStream::connect_timeout(&addr, Duration::from_millis(200)).is_ok() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

/// Assert the bare repo at `bare` fscks clean under system git (a real sha256
/// integrity cross-check of whatever the transport just wrote).
fn assert_fsck_clean(bare: &Path, when: &str) {
    let fsck = Command::new("git")
        .current_dir(bare)
        .args(["fsck", "--no-dangling"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("run git fsck");
    assert!(
        fsck.status.success(),
        "git fsck failed {when}: {}\n{}",
        String::from_utf8_lossy(&fsck.stdout),
        String::from_utf8_lossy(&fsck.stderr)
    );
}

// ---------------------------------------------------------------------------
// git:// daemon plumbing.
// ---------------------------------------------------------------------------

/// Spawn `git daemon` over `base_path` on `port`. `receive_pack` enables the
/// push service; `trace_path`, when set, captures the upload-pack pkt-lines so we
/// can prove v2 + sha256 negotiation on the wire.
fn spawn_daemon(
    base_path: &Path,
    port: u16,
    receive_pack: bool,
    trace_path: Option<&Path>,
) -> Option<Child> {
    let mut cmd = Command::new("git");
    cmd.arg("daemon")
        .arg("--listen=127.0.0.1")
        .arg(format!("--port={port}"))
        .arg("--reuseaddr")
        .arg("--export-all")
        .arg(format!("--base-path={}", base_path.display()))
        .arg(base_path)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if receive_pack {
        cmd.arg("--enable=receive-pack");
    }
    if let Some(tp) = trace_path {
        cmd.env("GIT_TRACE_PACKET", tp);
    }
    cmd.spawn().ok()
}

struct DaemonGuard(Child);
impl Drop for DaemonGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

// ---------------------------------------------------------------------------
// grit-http-server plumbing.
// ---------------------------------------------------------------------------

/// Locate a sibling binary (`grit`, `grit-http-server`) in the cargo target dir.
fn find_binary(name: &str) -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let deps = exe.parent()?;
    let profile = deps.parent()?;
    for cand in [profile.join(name), deps.join(name)] {
        if cand.is_file() {
            return Some(cand);
        }
    }
    None
}

fn spawn_server(server_bin: &Path, grit_bin: &Path, root: &Path, port: u16) -> Option<Child> {
    Command::new(server_bin)
        .arg("--root")
        .arg(root)
        .arg("--bind")
        .arg(format!("127.0.0.1:{port}"))
        .env("GUST_BIN", grit_bin)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .ok()
}

struct ServerGuard(Child);
impl Drop for ServerGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Resolve both the `grit` and `grit-http-server` binaries, or `None` (skip).
fn http_binaries() -> Option<(PathBuf, PathBuf)> {
    let grit = find_binary("grit")?;
    let server = find_binary("grit-http-server")?;
    Some((grit, server))
}

// ===========================================================================
// 1. fetch over git:// (protocol v2) into a sha256 local repo.
// ===========================================================================

#[test]
fn sha256_fetch_over_git_daemon_v2() {
    if !git_supports_sha256() {
        eprintln!("SKIP: local git cannot create sha256 repos");
        return;
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let work = tmp.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    build_sha256_source(&work);

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
    // The bare clone preserved the sha256 format.
    assert_eq!(
        git(&source, &["rev-parse", "--show-object-format"]).trim(),
        "sha256"
    );

    let main_oid = rev_parse(&source, "refs/heads/main");
    let topic_oid = rev_parse(&source, "refs/heads/topic");
    let c1_oid = rev_parse(&work, "HEAD~1");
    let tag_oid = rev_parse(&source, "refs/tags/v1");
    for (oid, what) in [
        (&main_oid, "main"),
        (&topic_oid, "topic"),
        (&c1_oid, "c1"),
        (&tag_oid, "tag"),
    ] {
        assert_sha256_oid(oid, what);
    }

    let Some(port) = free_port() else {
        eprintln!("SKIP: could not allocate a free port");
        return;
    };
    let trace_path = tmp.path().join("trace_packet.log");
    let Some(child) = spawn_daemon(&base, port, false, Some(&trace_path)) else {
        eprintln!("SKIP: `git daemon` is unavailable");
        return;
    };
    let _guard = DaemonGuard(child);
    if !wait_ready(port) {
        eprintln!("SKIP: git daemon did not become ready on port {port}");
        return;
    }

    let url = format!("git://127.0.0.1:{port}/repo.git");

    // Local sha256 repo so the library's odb hash-algo matches the wire format.
    let local = tmp.path().join("local");
    std::fs::create_dir_all(&local).unwrap();
    git(
        &local,
        &["init", "-q", "-b", "main", "--object-format=sha256", "."],
    );
    let local_git = local.join(".git");
    assert_eq!(open_odb(&local_git).hash_algo(), HashAlgo::Sha256);

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

    // v2 negotiated (no refs on connect; v2 capability block present).
    assert_eq!(
        conn.protocol_version(),
        2,
        "expected protocol v2 negotiation"
    );
    assert!(
        conn.advertised_refs().is_empty(),
        "a v2 connection advertises no refs on connect"
    );
    assert!(
        conn.capabilities().iter().any(|c| c == "ls-refs"
            || c.starts_with("ls-refs=")
            || c.starts_with("fetch=")
            || c == "fetch"),
        "v2 capability block missing ls-refs/fetch; got {:?}",
        conn.capabilities()
    );
    // The server advertised the sha256 object format in its capability block.
    assert!(
        conn.capabilities()
            .iter()
            .any(|c| c == "object-format=sha256"),
        "v2 capability block must advertise object-format=sha256; got {:?}",
        conn.capabilities()
    );

    let opts = FetchOptions {
        refspecs: vec!["+refs/heads/*:refs/remotes/origin/*".to_owned()],
        tags: TagMode::All,
        ..Default::default()
    };
    let outcome = fetch_remote(&local_git, &mut *conn, &opts, &mut NoProgress)
        .expect("sha256 v2 fetch_remote over git daemon");

    // Tracking refs landed with the source's 64-hex oids.
    let got_main = resolve_ref(&local_git, "refs/remotes/origin/main").expect("origin/main");
    let got_topic = resolve_ref(&local_git, "refs/remotes/origin/topic").expect("origin/topic");
    let got_tag = resolve_ref(&local_git, "refs/tags/v1").expect("tag v1");
    assert_eq!(got_main, main_oid, "origin/main oid mismatch");
    assert_eq!(got_topic, topic_oid, "origin/topic oid mismatch");
    assert_eq!(got_tag, tag_oid, "tag v1 oid mismatch");
    assert_sha256_oid(&got_main, "fetched main");

    // Objects landed in the local sha256 odb and are readable.
    let local_odb = open_odb(&local_git);
    assert_eq!(local_odb.hash_algo(), HashAlgo::Sha256);
    for oid in [main_oid, topic_oid, c1_oid, tag_oid] {
        assert!(
            local_odb.exists(&oid),
            "object {} missing from local sha256 odb after v2 fetch",
            oid.to_hex()
        );
        local_odb
            .read(&oid)
            .unwrap_or_else(|e| panic!("read {}: {e}", oid.to_hex()));
    }

    // Per-ref update + default branch from the v2 ls-refs HEAD symref.
    let main_update = outcome
        .updates
        .iter()
        .find(|u| u.remote_ref == "refs/heads/main")
        .expect("update for main");
    assert_eq!(main_update.mode, UpdateMode::New);
    assert_eq!(main_update.new_oid, Some(main_oid));
    assert_eq!(outcome.default_branch.as_deref(), Some("main"));

    // fsck clean (system git cross-check on the fetched sha256 pack).
    assert_fsck_clean(&local_git, "after sha256 v2 fetch");

    // Wire proof: the daemon's upload-pack child ran the v2 commands AND the
    // advertisement announced object-format=sha256 (not sha1).
    let trace = std::fs::read_to_string(&trace_path).unwrap_or_default();
    assert!(
        !trace.is_empty(),
        "GIT_TRACE_PACKET produced no output; cannot confirm negotiation"
    );
    assert!(
        trace.contains("version 2"),
        "trace missing `version 2` advertisement:\n{trace}"
    );
    assert!(
        trace.contains("command=ls-refs"),
        "trace missing `command=ls-refs`:\n{trace}"
    );
    assert!(
        trace.contains("command=fetch"),
        "trace missing `command=fetch`:\n{trace}"
    );
    assert!(
        trace.contains("object-format=sha256"),
        "trace missing `object-format=sha256` (sha256 not negotiated on the wire):\n{trace}"
    );
}

// ===========================================================================
// 2. fetch over smart-HTTP (v0/v1 stateless RPC) into a sha256 local repo.
// ===========================================================================

#[test]
fn sha256_fetch_over_smart_http() {
    if !git_supports_sha256() {
        eprintln!("SKIP: local git cannot create sha256 repos");
        return;
    }
    let Some((grit_bin, server_bin)) = http_binaries() else {
        eprintln!("SKIP: grit / grit-http-server binary not found (build them first)");
        return;
    };

    let tmp = tempfile::tempdir().expect("tempdir");
    let work = tmp.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    build_sha256_source(&work);

    let root = tmp.path().join("srv");
    std::fs::create_dir_all(&root).unwrap();
    let source = root.join("repo.git");
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
    assert_eq!(
        git(&source, &["rev-parse", "--show-object-format"]).trim(),
        "sha256"
    );

    let main_oid = rev_parse(&source, "refs/heads/main");
    let topic_oid = rev_parse(&source, "refs/heads/topic");
    let c1_oid = rev_parse(&work, "HEAD~1");
    let tag_oid = rev_parse(&source, "refs/tags/v1");
    assert_sha256_oid(&main_oid, "source main");

    let Some(port) = free_port() else {
        eprintln!("SKIP: could not allocate a free port");
        return;
    };
    let Some(child) = spawn_server(&server_bin, &grit_bin, &root, port) else {
        eprintln!("SKIP: could not spawn grit-http-server");
        return;
    };
    let _guard = ServerGuard(child);
    if !wait_ready(port) {
        eprintln!("SKIP: grit-http-server did not become ready on port {port}");
        return;
    }

    let url = format!("http://127.0.0.1:{port}/repo.git");

    // Empty local sha256 repo.
    let local = tmp.path().join("local");
    std::fs::create_dir_all(&local).unwrap();
    git(
        &local,
        &["init", "-q", "-b", "main", "--object-format=sha256", "."],
    );
    let local_git = local.join(".git");
    assert_eq!(open_odb(&local_git).hash_algo(), HashAlgo::Sha256);

    // 1. Advertisement: connect and confirm refs + the sha256 capability.
    let client = UreqHttpClient::new();
    let transport = SmartHttpTransport::new(client);
    let conn = match transport.connect(&url, Service::UploadPack, &ConnectOptions::default()) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("SKIP: could not connect to grit-http-server: {e}");
            return;
        }
    };
    assert!(
        conn.advertised_refs()
            .iter()
            .any(|(n, o)| n == "refs/heads/main" && *o == main_oid),
        "advertisement missing refs/heads/main = {}",
        main_oid.to_hex()
    );
    assert_eq!(conn.head_symref(), Some("refs/heads/main"));
    assert!(
        conn.capabilities()
            .iter()
            .any(|c| c == "object-format=sha256"),
        "v0/v1 advertisement must announce object-format=sha256; got {:?}",
        conn.capabilities()
    );
    drop(conn);

    // 2. Fetch via http_fetch.
    let client = UreqHttpClient::new();
    let opts = FetchOptions {
        refspecs: vec!["+refs/heads/*:refs/remotes/origin/*".to_owned()],
        tags: TagMode::All,
        ..Default::default()
    };
    let outcome = http_fetch(&client, &local_git, &url, &opts, &mut NoProgress)
        .expect("sha256 http_fetch over grit-http-server");

    let got_main = resolve_ref(&local_git, "refs/remotes/origin/main").expect("origin/main");
    let got_topic = resolve_ref(&local_git, "refs/remotes/origin/topic").expect("origin/topic");
    let got_tag = resolve_ref(&local_git, "refs/tags/v1").expect("tag v1");
    assert_eq!(got_main, main_oid, "origin/main oid mismatch");
    assert_eq!(got_topic, topic_oid, "origin/topic oid mismatch");
    assert_eq!(got_tag, tag_oid, "tag v1 oid mismatch");
    assert_sha256_oid(&got_main, "fetched main");

    let local_odb = open_odb(&local_git);
    assert_eq!(local_odb.hash_algo(), HashAlgo::Sha256);
    for oid in [main_oid, topic_oid, c1_oid, tag_oid] {
        assert!(
            local_odb.exists(&oid),
            "object {} missing from local sha256 odb after http fetch",
            oid.to_hex()
        );
        local_odb
            .read(&oid)
            .unwrap_or_else(|e| panic!("read {}: {e}", oid.to_hex()));
    }

    let main_update = outcome
        .updates
        .iter()
        .find(|u| u.remote_ref == "refs/heads/main")
        .expect("update for main");
    assert_eq!(main_update.mode, UpdateMode::New);
    assert_eq!(main_update.new_oid, Some(main_oid));
    assert_eq!(outcome.default_branch.as_deref(), Some("main"));

    // Cross-check the fetched tip against system git and fsck the local repo.
    assert_eq!(
        got_main.to_hex(),
        git(&source, &["rev-parse", "refs/heads/main"]).trim()
    );
    assert_fsck_clean(&local_git, "after sha256 http fetch");
}

// ===========================================================================
// 3. push over git:// (v0/v1 receive-pack) into an empty sha256 bare repo.
// ===========================================================================

#[test]
fn sha256_push_over_git_daemon() {
    if !git_supports_sha256() {
        eprintln!("SKIP: local git cannot create sha256 repos");
        return;
    }

    let tmp = tempfile::tempdir().expect("tempdir");

    // Local sha256 source repo with two commits on main.
    let local = tmp.path().join("local");
    std::fs::create_dir_all(&local).unwrap();
    build_sha256_source(&local);
    let local_git = local.join(".git");
    let main_oid = rev_parse(&local, "refs/heads/main");
    let c1_oid = rev_parse(&local, "HEAD~1");
    assert_sha256_oid(&main_oid, "local main");

    // Empty sha256 bare remote under the daemon base path, served at /repo.git.
    let base = tmp.path().join("srv");
    std::fs::create_dir_all(&base).unwrap();
    let bare = base.join("repo.git");
    std::fs::create_dir_all(&bare).unwrap();
    git(
        &bare,
        &["init", "-q", "--bare", "--object-format=sha256", "."],
    );
    git(&bare, &["config", "daemon.receivepack", "true"]);
    assert_eq!(
        git(&bare, &["rev-parse", "--show-object-format"]).trim(),
        "sha256",
        "bare receive target must be sha256"
    );

    let Some(port) = free_port() else {
        eprintln!("SKIP: could not allocate a free port");
        return;
    };
    let Some(child) = spawn_daemon(&base, port, true, None) else {
        eprintln!("SKIP: `git daemon` is unavailable");
        return;
    };
    let _guard = DaemonGuard(child);
    if !wait_ready(port) {
        eprintln!("SKIP: git daemon did not become ready on port {port}");
        return;
    }

    let url = format!("git://127.0.0.1:{port}/repo.git");
    let transport = GitDaemonTransport::new();

    // --- 1. Create push of refs/heads/main ------------------------------------
    let mut conn = match transport.connect(&url, Service::ReceivePack, &ConnectOptions::default()) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("SKIP: could not connect to git daemon receive-pack: {e}");
            return;
        }
    };
    // The empty sha256 remote advertises object-format=sha256 even with no refs.
    assert!(
        conn.capabilities()
            .iter()
            .any(|c| c == "object-format=sha256"),
        "receive-pack advertisement must announce object-format=sha256; got {:?}",
        conn.capabilities()
    );

    let spec = PushRefSpec {
        src: Some(main_oid),
        dst: "refs/heads/main".to_owned(),
        force: false,
        delete: false,
        expected_old: None,
        expect_absent: false,
    };
    let outcome = push_remote(
        &local_git,
        &mut *conn,
        &[spec],
        &PushOptions::default(),
        &mut NoProgress,
    )
    .expect("sha256 push_remote over git daemon");
    drop(conn);

    assert_eq!(outcome.results.len(), 1);
    let r = &outcome.results[0];
    assert_eq!(
        r.status,
        PushRefStatus::Ok,
        "sha256 create push should be accepted, got {:?} ({:?})",
        r.status,
        r.message
    );
    assert_eq!(r.new_oid, Some(main_oid));
    assert!(r.old_oid.is_none(), "new ref has no old value");

    // The remote ref + objects landed with 64-hex oids; system git agrees.
    let remote_main = resolve_ref(&bare, "refs/heads/main").expect("remote main written");
    assert_eq!(remote_main, main_oid, "remote main oid mismatch");
    assert_sha256_oid(&remote_main, "remote main");
    assert_eq!(
        remote_main.to_hex(),
        git(&bare, &["rev-parse", "refs/heads/main"]).trim()
    );

    let remote_odb = open_odb(&bare);
    assert_eq!(remote_odb.hash_algo(), HashAlgo::Sha256);
    for oid in [main_oid, c1_oid] {
        assert!(
            remote_odb.exists(&oid),
            "object {} missing from remote sha256 odb after push",
            oid.to_hex()
        );
        remote_odb
            .read(&oid)
            .unwrap_or_else(|e| panic!("read {}: {e}", oid.to_hex()));
    }
    assert_fsck_clean(&bare, "after sha256 git:// push");

    // --- 2. Non-fast-forward push (no force) is rejected per-ref ---------------
    git(&local, &["checkout", "-q", "-b", "diverge", "HEAD~1"]);
    std::fs::write(local.join("c.txt"), "three\n").unwrap();
    git(&local, &["add", "c.txt"]);
    git(&local, &["commit", "-q", "-m", "divergent"]);
    let diverged = rev_parse(&local, "HEAD");
    assert_ne!(diverged, main_oid);
    assert_sha256_oid(&diverged, "divergent");

    let mut conn2 = transport
        .connect(&url, Service::ReceivePack, &ConnectOptions::default())
        .expect("reconnect for non-ff push");
    assert!(
        conn2
            .advertised_refs()
            .iter()
            .any(|(n, o)| n == "refs/heads/main" && *o == main_oid),
        "reconnect advertisement should report remote main at {}",
        main_oid.to_hex()
    );
    let nonff = PushRefSpec {
        src: Some(diverged),
        dst: "refs/heads/main".to_owned(),
        force: false,
        delete: false,
        expected_old: None,
        expect_absent: false,
    };
    let outcome2 = push_remote(
        &local_git,
        &mut *conn2,
        &[nonff],
        &PushOptions::default(),
        &mut NoProgress,
    )
    .expect("non-ff push_remote completes");
    drop(conn2);
    assert_eq!(
        outcome2.results[0].status,
        PushRefStatus::RejectNonFastForward,
        "non-fast-forward sha256 push must be rejected"
    );
    assert_eq!(
        resolve_ref(&bare, "refs/heads/main").expect("remote main still set"),
        main_oid,
        "rejected non-ff push must not move the remote sha256 ref"
    );

    // --- 3. Forced update is accepted and advances the ref --------------------
    let mut conn3 = transport
        .connect(&url, Service::ReceivePack, &ConnectOptions::default())
        .expect("reconnect for forced push");
    let forced = PushRefSpec {
        src: Some(diverged),
        dst: "refs/heads/main".to_owned(),
        force: true,
        delete: false,
        expected_old: None,
        expect_absent: false,
    };
    let outcome3 = push_remote(
        &local_git,
        &mut *conn3,
        &[forced],
        &PushOptions::default(),
        &mut NoProgress,
    )
    .expect("forced push_remote completes");
    drop(conn3);
    let r3 = &outcome3.results[0];
    assert_eq!(
        r3.status,
        PushRefStatus::Ok,
        "forced sha256 push should be accepted, got {:?} ({:?})",
        r3.status,
        r3.message
    );
    assert!(r3.forced, "forced update should be flagged forced");
    assert_eq!(r3.new_oid, Some(diverged));
    assert_eq!(
        resolve_ref(&bare, "refs/heads/main").expect("remote main after force"),
        diverged,
        "forced sha256 push must advance the remote ref"
    );
    assert!(
        open_odb(&bare).exists(&diverged),
        "divergent object {} missing after forced sha256 push",
        diverged.to_hex()
    );
    assert_fsck_clean(&bare, "after forced sha256 git:// push");
}

// ===========================================================================
// 4. push over smart-HTTP (v0/v1 receive-pack) into an empty sha256 bare repo.
// ===========================================================================

#[test]
fn sha256_push_over_smart_http() {
    if !git_supports_sha256() {
        eprintln!("SKIP: local git cannot create sha256 repos");
        return;
    }
    let Some((grit_bin, server_bin)) = http_binaries() else {
        eprintln!("SKIP: grit / grit-http-server binary not found (build them first)");
        return;
    };

    let tmp = tempfile::tempdir().expect("tempdir");

    // Local sha256 source repo with two commits on main.
    let local = tmp.path().join("local");
    std::fs::create_dir_all(&local).unwrap();
    build_sha256_source(&local);
    let local_git = local.join(".git");
    let main_oid = rev_parse(&local, "refs/heads/main");
    let c1_oid = rev_parse(&local, "HEAD~1");
    assert_sha256_oid(&main_oid, "local main");

    // Empty sha256 bare receive target served at /push.git under the server root.
    let root = tmp.path().join("srv");
    std::fs::create_dir_all(&root).unwrap();
    let bare = root.join("push.git");
    std::fs::create_dir_all(&bare).unwrap();
    git(
        &bare,
        &["init", "-q", "--bare", "--object-format=sha256", "."],
    );
    git(&bare, &["symbolic-ref", "HEAD", "refs/heads/main"]);
    assert_eq!(
        git(&bare, &["rev-parse", "--show-object-format"]).trim(),
        "sha256",
        "bare receive target must be sha256"
    );

    let Some(port) = free_port() else {
        eprintln!("SKIP: could not allocate a free port");
        return;
    };
    let Some(child) = spawn_server(&server_bin, &grit_bin, &root, port) else {
        eprintln!("SKIP: could not spawn grit-http-server");
        return;
    };
    let _guard = ServerGuard(child);
    if !wait_ready(port) {
        eprintln!("SKIP: grit-http-server did not become ready on port {port}");
        return;
    }

    let url = format!("http://127.0.0.1:{port}/push.git");

    // Confirm the server offers receive-pack AND advertises sha256 over HTTP; skip
    // cleanly if the service is absent (matches the sibling tests' policy).
    let client = UreqHttpClient::new();
    let probe_url = format!("{url}/info/refs?service=git-receive-pack");
    match client.get(&probe_url, None) {
        Ok(body) => {
            if !body.windows(20).any(|w| w == b"# service=git-receiv") {
                eprintln!("SKIP: server returned a non-smart receive-pack advertisement");
                return;
            }
            assert!(
                body.windows(b"object-format=sha256".len())
                    .any(|w| w == b"object-format=sha256"),
                "http receive-pack advertisement must announce object-format=sha256"
            );
        }
        Err(e) => {
            eprintln!("SKIP: server does not offer receive-pack: {e}");
            return;
        }
    }

    // --- 1. Create push of refs/heads/main ------------------------------------
    let spec = PushRefSpec {
        src: Some(main_oid),
        dst: "refs/heads/main".to_owned(),
        force: false,
        delete: false,
        expected_old: None,
        expect_absent: false,
    };
    let outcome = push_http(
        &client,
        &local_git,
        &url,
        &[spec],
        &PushOptions::default(),
        &mut NoProgress,
    )
    .expect("sha256 push_http over grit-http-server");

    assert_eq!(outcome.results.len(), 1);
    let r = &outcome.results[0];
    assert_eq!(
        r.status,
        PushRefStatus::Ok,
        "sha256 http create push should be accepted, got {:?} ({:?})",
        r.status,
        r.message
    );
    assert_eq!(r.new_oid, Some(main_oid));
    assert!(r.old_oid.is_none(), "new ref has no old value");

    let remote_main = resolve_ref(&bare, "refs/heads/main").expect("remote main written");
    assert_eq!(
        remote_main, main_oid,
        "remote main oid mismatch after http push"
    );
    assert_sha256_oid(&remote_main, "remote main");
    assert_eq!(
        remote_main.to_hex(),
        git(&bare, &["rev-parse", "refs/heads/main"]).trim()
    );

    let remote_odb = open_odb(&bare);
    assert_eq!(remote_odb.hash_algo(), HashAlgo::Sha256);
    for oid in [main_oid, c1_oid] {
        assert!(
            remote_odb.exists(&oid),
            "object {} missing from remote sha256 odb after http push",
            oid.to_hex()
        );
        remote_odb
            .read(&oid)
            .unwrap_or_else(|e| panic!("read {}: {e}", oid.to_hex()));
    }
    assert_fsck_clean(&bare, "after sha256 http push");

    // --- 2. Push a second branch over the now-populated sha256 remote ---------
    // Exercises the thin/delta pack path against the advertised sha256 tips.
    git(&local, &["checkout", "-q", "-b", "feature"]);
    std::fs::write(local.join("t.txt"), "feature\n").unwrap();
    git(&local, &["add", "t.txt"]);
    git(&local, &["commit", "-q", "-m", "feature1"]);
    let feature_oid = rev_parse(&local, "refs/heads/feature");
    assert_sha256_oid(&feature_oid, "feature");

    let spec_feature = PushRefSpec {
        src: Some(feature_oid),
        dst: "refs/heads/feature".to_owned(),
        force: false,
        delete: false,
        expected_old: None,
        expect_absent: false,
    };
    let outcome_feature = push_http(
        &client,
        &local_git,
        &url,
        &[spec_feature],
        &PushOptions::default(),
        &mut NoProgress,
    )
    .expect("sha256 push_http feature over grit-http-server");
    assert_eq!(
        outcome_feature.results[0].status,
        PushRefStatus::Ok,
        "second sha256 http push should be accepted, got {:?} ({:?})",
        outcome_feature.results[0].status,
        outcome_feature.results[0].message
    );
    let remote_feature = resolve_ref(&bare, "refs/heads/feature").expect("remote feature written");
    assert_eq!(remote_feature, feature_oid);
    assert!(
        remote_odb.exists(&feature_oid),
        "feature object {} missing after second sha256 http push",
        feature_oid.to_hex()
    );
    assert_fsck_clean(&bare, "after second sha256 http push");
}
