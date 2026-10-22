//! ADVERSARIAL verification of protocol-v2 fetch over the `git://` streaming
//! transport. The goal of this suite is to *disprove* the claim that v2 is
//! genuinely negotiated and that the negotiation behaves correctly — and, when
//! it cannot be disproved, to lock the wire behavior in as a permanent guard.
//!
//! Every test drives a real `git daemon` with `GIT_TRACE_PACKET` writing to a
//! file, then inspects the captured pkt-lines (the upload-pack child's view) to
//! prove, from the actual bytes on the wire:
//!
//!   * v2 was negotiated and NO v0/v1 ref advertisement was used (the strongest
//!     anti-fallback proof: a v0/v1 upload-pack emits a `<oid> HEAD\0<caps>` ref
//!     advertisement as its FIRST packet; a v2 upload-pack never does — it emits
//!     `version 2` then answers `command=ls-refs` / `command=fetch`);
//!   * incremental fetch with shared history actually offers `have` lines AND
//!     produces a *minimal* pack (not the full closure);
//!   * a from-scratch clone (empty have set) sends no `have` lines;
//!   * an empty / unborn-HEAD remote yields an empty, hang-free fetch;
//!   * tag fetching over v2 (`TagMode::All` and `Following`);
//!   * ref-prefix filtering: fetching only `refs/heads/*` does NOT advertise
//!     `refs/tags/*` from ls-refs (asserted both on the wire request and in the
//!     parsed ref set).
//!
//! Every fetched oid is cross-checked against system `git rev-parse` and the
//! result is `git fsck`-ed.
//!
//! Skips gracefully (returns early) when `git daemon` is unavailable or fails to
//! bind.

use std::cell::RefCell;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::rc::Rc;
use std::time::{Duration, Instant};

use grit_lib::objects::ObjectId;
use grit_lib::refs::resolve_ref;
use grit_lib::remote::fetch::{fetch_remote, NoProgress};
use grit_lib::remote::transfer::{FetchOptions, TagMode};
use grit_lib::remote::transport::{
    ConnectOptions, Connection, GitDaemonTransport, Service, Transport,
};

/// A [`Connection`] decorator that tees every byte the server sends into a shared
/// buffer, so a test can inspect the raw server->client stream after the fetch —
/// in particular to find the `PACK` magic and read the pack's object count (the
/// big-endian u32 at offset 8 of the header). This is the definitive proof that
/// negotiation produced a MINIMAL pack: a broken negotiation makes the server
/// pack the full closure, inflating that count (the v2 pack arrives side-band
/// framed, so we scan the captured stream for `PACK` rather than assume offset).
struct TeeConn {
    inner: Box<dyn Connection>,
    captured: Rc<RefCell<Vec<u8>>>,
    // Holds the per-call tee reader alive so the `&mut dyn Read` we hand out
    // remains valid for the duration of the borrow.
    tee_reader_slot: Option<TeeReader>,
}

/// Wraps the inner connection's reader by raw pointer. Sound because the inner
/// `Box<dyn Connection>` is pinned in `TeeConn` (never moved while a reader is
/// outstanding), and the test hands out only one reader at a time.
struct TeeReader {
    inner: *mut dyn Connection,
    captured: Rc<RefCell<Vec<u8>>>,
}

impl Read for TeeReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        // SAFETY: `inner` points at the `Box<dyn Connection>` owned by the
        // `TeeConn`, which outlives every `TeeReader` it creates; only one reader
        // is live at a time.
        let conn: &mut dyn Connection = unsafe { &mut *self.inner };
        let n = conn.reader().read(buf)?;
        if n > 0 {
            self.captured.borrow_mut().extend_from_slice(&buf[..n]);
        }
        Ok(n)
    }
}

impl TeeConn {
    fn new(inner: Box<dyn Connection>) -> Self {
        TeeConn {
            inner,
            captured: Rc::new(RefCell::new(Vec::new())),
            tee_reader_slot: None,
        }
    }
    fn captured(&self) -> Rc<RefCell<Vec<u8>>> {
        Rc::clone(&self.captured)
    }
}

impl Connection for TeeConn {
    fn reader(&mut self) -> &mut dyn Read {
        let ptr: *mut dyn Connection = self.inner.as_mut();
        self.tee_reader_slot = Some(TeeReader {
            inner: ptr,
            captured: Rc::clone(&self.captured),
        });
        self.tee_reader_slot.as_mut().unwrap()
    }
    fn writer(&mut self) -> &mut dyn Write {
        self.inner.writer()
    }
    fn advertised_refs(&self) -> &[(String, ObjectId)] {
        self.inner.advertised_refs()
    }
    fn capabilities(&self) -> &[String] {
        self.inner.capabilities()
    }
    fn head_symref(&self) -> Option<&str> {
        self.inner.head_symref()
    }
    fn protocol_version(&self) -> u8 {
        self.inner.protocol_version()
    }
    fn finish_send(&mut self) {
        self.inner.finish_send();
    }
}

/// Find the `PACK` magic in `stream` and return the pack header's object count
/// (the big-endian u32 at header offset 8). Returns `None` if no PACK header is
/// present (e.g. an empty/no-op fetch). The v2 pack is side-band framed, but the
/// `PACK` magic and the following 8 header bytes land contiguously inside the
/// first channel-1 data packet, so a raw scan of the captured stream recovers
/// them. (We take the LAST occurrence to avoid a spurious match inside the
/// pkt-line framing of earlier sections.)
fn pack_object_count(stream: &[u8]) -> Option<u32> {
    let mut pos = None;
    let mut i = 0;
    while i + 12 <= stream.len() {
        if &stream[i..i + 4] == b"PACK" {
            pos = Some(i);
        }
        i += 1;
    }
    let p = pos?;
    if p + 12 > stream.len() {
        return None;
    }
    let cnt = u32::from_be_bytes([stream[p + 8], stream[p + 9], stream[p + 10], stream[p + 11]]);
    Some(cnt)
}

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

fn spawn_daemon(base_path: &Path, port: u16, trace_path: &Path) -> Option<Child> {
    Command::new("git")
        .arg("daemon")
        .arg("--listen=127.0.0.1")
        .arg(format!("--port={port}"))
        .arg("--reuseaddr")
        .arg("--export-all")
        .arg(format!("--base-path={}", base_path.display()))
        .arg(base_path)
        .env("GIT_TRACE_PACKET", trace_path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .ok()
}

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

/// Read the trace file, retrying briefly because the upload-pack child flushes
/// its trace asynchronously after our socket closes.
fn read_trace_settled(path: &Path) -> String {
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut last = String::new();
    loop {
        let now = std::fs::read_to_string(path).unwrap_or_default();
        if !now.is_empty() && now == last && now.contains("0000") {
            return now;
        }
        if Instant::now() >= deadline {
            return now;
        }
        last = now;
        std::thread::sleep(Duration::from_millis(80));
    }
}

/// The strongest anti-fallback assertion: a v0/v1 upload-pack advertises refs as
/// its FIRST packet (`<oid> HEAD\0<caps...>` containing `multi_ack` /
/// `symref=HEAD:`), and never writes `command=ls-refs`. A v2 upload-pack writes
/// `version 2` and answers `command=ls-refs` / `command=fetch`, and NEVER writes
/// the `... HEAD\0...multi_ack...` v0 advertisement line. We assert both the
/// positive v2 markers and the *absence* of the v0 advertisement.
fn assert_trace_is_v2_not_v0(trace: &str) {
    assert!(
        !trace.is_empty(),
        "GIT_TRACE_PACKET empty; cannot prove v2 negotiation"
    );
    assert!(
        trace.contains("version 2"),
        "trace missing `version 2`:\n{trace}"
    );
    assert!(
        trace.contains("command=ls-refs"),
        "trace missing `command=ls-refs`:\n{trace}"
    );
    // NEGATIVE proof: no v0/v1 ref advertisement. A v0 advertisement line carries
    // the capability list with `symref=HEAD:` and `multi_ack` jammed after a NUL
    // on the HEAD line. v2 never emits this.
    for line in trace.lines() {
        // The trace renders the NUL as a literal `\0`.
        if line.contains("HEAD\\0") && line.contains("symref=HEAD:") {
            panic!("found a v0/v1 HEAD ref-advertisement line (silent fallback!):\n{line}");
        }
        if line.contains("HEAD\\0") && line.contains("multi_ack") {
            panic!("found a v0/v1 capabilities-on-HEAD advertisement (fallback!):\n{line}");
        }
    }
}

fn fsck_clean(local_root: &Path) {
    let fsck = Command::new("git")
        .current_dir(local_root)
        .args(["fsck", "--no-dangling"])
        .output()
        .expect("run git fsck");
    assert!(
        fsck.status.success(),
        "git fsck failed: {}\n{}",
        String::from_utf8_lossy(&fsck.stdout),
        String::from_utf8_lossy(&fsck.stderr)
    );
}

/// Bundle the daemon setup so each test is a few lines. Returns `None` (caller
/// returns early / SKIP) when the daemon is unavailable.
struct Server {
    _tmp: tempfile::TempDir,
    base: std::path::PathBuf,
    source: std::path::PathBuf,
    trace_path: std::path::PathBuf,
    url: String,
    _guard: DaemonGuard,
}

impl Server {
    /// The daemon process id, for the unborn-remote test's hang watchdog.
    fn daemon_pid(&self) -> u32 {
        self._guard.0.id()
    }
}

/// Force-kill a process by pid (the unborn-remote watchdog). Best-effort.
fn kill_pid(pid: u32) {
    let _ = Command::new("kill").arg("-9").arg(pid.to_string()).status();
}

fn start_server_with(build: impl FnOnce(&Path, &Path)) -> Option<Server> {
    let tmp = tempfile::tempdir().expect("tempdir");
    let work = tmp.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    let base = tmp.path().join("srv");
    std::fs::create_dir_all(&base).unwrap();
    let source = base.join("repo.git");
    build(&work, &source);

    let port = free_port()?;
    let trace_path = tmp.path().join("trace_packet.log");
    let child = spawn_daemon(&base, port, &trace_path)?;
    let guard = DaemonGuard(child);
    if !wait_ready(port) {
        eprintln!("SKIP: git daemon did not become ready on port {port}");
        return None;
    }
    let url = format!("git://127.0.0.1:{port}/repo.git");
    Some(Server {
        _tmp: tmp,
        base,
        source,
        trace_path,
        url,
        _guard: guard,
    })
}

fn truncate_trace(path: &Path) {
    let _ = std::fs::write(path, b"");
}

// ---------------------------------------------------------------------------
// 1. From-scratch clone (empty have set): v2 negotiated, NO v0 advertisement,
//    NO `have` lines on the wire.
// ---------------------------------------------------------------------------
#[test]
fn v2_clone_from_scratch_no_haves_and_not_v0() {
    let Some(srv) = start_server_with(|work, source| {
        build_source(work);
        git(
            work,
            &["clone", "-q", "--bare", ".", source.to_str().unwrap()],
        );
        git(source, &["symbolic-ref", "HEAD", "refs/heads/main"]);
    }) else {
        eprintln!("SKIP: `git daemon` is unavailable");
        return;
    };

    let main_oid = rev_parse(&srv.source, "refs/heads/main");
    let topic_oid = rev_parse(&srv.source, "refs/heads/topic");

    let tmp = tempfile::tempdir().unwrap();
    let local = tmp.path().join("local");
    std::fs::create_dir_all(&local).unwrap();
    git(&local, &["init", "-q", "-b", "main", "."]);
    let local_git = local.join(".git");

    let transport = GitDaemonTransport::new();
    let opts_v2 = ConnectOptions {
        protocol_version: 2,
        ..Default::default()
    };
    let mut conn = match transport.connect(&srv.url, Service::UploadPack, &opts_v2) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("SKIP: could not connect: {e}");
            return;
        }
    };
    assert_eq!(conn.protocol_version(), 2, "expected v2 negotiation");
    assert!(conn.advertised_refs().is_empty(), "v2 advertises no refs");

    let mut opts = FetchOptions {
        refspecs: vec!["+refs/heads/*:refs/remotes/origin/*".to_owned()],
        tags: TagMode::None,
        ..Default::default()
    };
    fetch_remote(&local_git, &mut *conn, &mut opts, &mut NoProgress).expect("v2 clone");
    drop(conn);

    assert_eq!(
        resolve_ref(&local_git, "refs/remotes/origin/main").unwrap(),
        main_oid
    );
    assert_eq!(
        resolve_ref(&local_git, "refs/remotes/origin/topic").unwrap(),
        topic_oid
    );
    fsck_clean(&local);

    let trace = read_trace_settled(&srv.trace_path);
    assert_trace_is_v2_not_v0(&trace);
    assert!(
        trace.contains("command=fetch"),
        "trace missing `command=fetch`:\n{trace}"
    );
    // From an empty local repo there is nothing to offer: the client must send NO
    // `have` line. A `have ` on the wire here would mean we fabricated history.
    for line in trace.lines() {
        // Match a client->server `have <oid>` (the trace prefixes direction; we
        // look at the request side, which the upload-pack child reads).
        if let Some(idx) = line.find("have ") {
            let after = &line[idx + 5..];
            if after.chars().take(40).all(|c| c.is_ascii_hexdigit()) && after.len() >= 40 {
                panic!("from-scratch clone sent a `have` line (fabricated history):\n{line}");
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 2. Incremental fetch with shared history: `have` lines fire AND the pack is
//    minimal (only the genuinely-new objects), not the full closure.
// ---------------------------------------------------------------------------
#[test]
fn v2_incremental_offers_haves_and_pack_is_minimal() {
    let Some(srv) = start_server_with(|work, source| {
        build_source(work);
        git(work, &["checkout", "-q", "main"]);
        // Deepen main past the 16-have initial-flush window so a multi-round
        // exchange is forced.
        for i in 0..25 {
            git(
                work,
                &["commit", "-q", "--allow-empty", "-m", &format!("d{i}")],
            );
        }
        git(
            work,
            &["clone", "-q", "--bare", ".", source.to_str().unwrap()],
        );
        git(source, &["symbolic-ref", "HEAD", "refs/heads/main"]);
    }) else {
        eprintln!("SKIP: `git daemon` is unavailable");
        return;
    };

    let transport = GitDaemonTransport::new();
    let opts_v2 = ConnectOptions {
        protocol_version: 2,
        ..Default::default()
    };

    let tmp = tempfile::tempdir().unwrap();
    let local = tmp.path().join("local");
    std::fs::create_dir_all(&local).unwrap();
    git(&local, &["init", "-q", "-b", "main", "."]);
    let local_git = local.join(".git");

    // Seed: full fetch of main's deep history.
    {
        let mut conn = match transport.connect(&srv.url, Service::UploadPack, &opts_v2) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("SKIP: could not connect: {e}");
                return;
            }
        };
        assert_eq!(conn.protocol_version(), 2);
        let mut opts = FetchOptions {
            refspecs: vec!["+refs/heads/main:refs/remotes/origin/main".to_owned()],
            tags: TagMode::None,
            ..Default::default()
        };
        fetch_remote(&local_git, &mut *conn, &mut opts, &mut NoProgress).expect("seed fetch");
    }
    // Local branch at the seeded tip so the negotiator has a real head (main's
    // full deep history) to offer as `have`s.
    git(&local, &["branch", "base", "refs/remotes/origin/main"]);

    // Add ONE new commit on top of remote `main` (whose entire ~30-object history
    // the local already holds). After our `have`s, the minimal pack the server
    // sends is just the new commit + its (one) tree + its new blob ~= 3 objects.
    // The FULL closure of `main` is ~30 objects. The server records the count it
    // packed in the PACK header (offset 8), so reading it off the wire
    // distinguishes a real negotiation from a full re-download — a distinction the
    // resulting on-disk object count CANNOT make, because unpack dedups against
    // objects we already hold.
    let work2 = srv.base.parent().unwrap().join("work2");
    git(
        &srv.source,
        &["worktree", "add", "-q", work2.to_str().unwrap(), "main"],
    );
    std::fs::write(work2.join("c.txt"), "three\n").unwrap();
    git(&work2, &["add", "c.txt"]);
    git(&work2, &["commit", "-q", "-m", "c3"]);
    let main_new = rev_parse(&srv.source, "refs/heads/main");

    // The number of objects in `main`'s full closure (what a broken negotiation
    // would re-download). Used as the upper bound the minimal pack must beat.
    let main_closure: u32 = git(&srv.source, &["rev-list", "--objects", "main"])
        .lines()
        .count() as u32;
    assert!(
        main_closure >= 25,
        "test setup: main closure should be large, got {main_closure}"
    );

    truncate_trace(&srv.trace_path);

    // Incremental fetch: local already holds main's deep history, so it must offer
    // `have`s and pull a MINIMAL pack. Tee the server->client bytes so we can read
    // the PACK header's object count.
    let raw_conn = transport
        .connect(&srv.url, Service::UploadPack, &opts_v2)
        .expect("reconnect");
    let mut conn = TeeConn::new(raw_conn);
    let captured = conn.captured();
    assert_eq!(conn.protocol_version(), 2);
    let mut opts = FetchOptions {
        refspecs: vec!["+refs/heads/main:refs/remotes/origin/main".to_owned()],
        tags: TagMode::None,
        ..Default::default()
    };
    fetch_remote(&local_git, &mut conn, &mut opts, &mut NoProgress).expect("incremental fetch");
    drop(conn);

    assert_eq!(
        resolve_ref(&local_git, "refs/remotes/origin/main").unwrap(),
        main_new,
        "incremental main tip"
    );
    fsck_clean(&local);

    // DEFINITIVE minimality proof: the PACK header object count the server sent.
    let stream = captured.borrow();
    let packed = pack_object_count(&stream)
        .expect("a PACK header should be present on the wire for an incremental fetch");
    // A real thin-pack for one new commit packs only a handful of objects; the
    // full closure is `main_closure` (~30). We require the packed count to be
    // well under the closure (and tiny in absolute terms) — a full re-download
    // would report ~main_closure here.
    assert!(
        packed <= 8 && packed < main_closure,
        "incremental v2 fetch was NOT minimal: server packed {packed} objects \
         (main full closure is {main_closure}); a working have/ready negotiation \
         should pack only the new commit + tree + blob"
    );

    // And the negotiation genuinely happened on the wire.
    let trace = read_trace_settled(&srv.trace_path);
    assert_trace_is_v2_not_v0(&trace);
    assert!(
        trace.contains("command=fetch"),
        "incremental trace missing `command=fetch`:\n{trace}"
    );
    let saw_have = trace.lines().any(|line| {
        line.find("have ").is_some_and(|idx| {
            let after = &line[idx + 5..];
            after.len() >= 40 && after.chars().take(40).all(|c| c.is_ascii_hexdigit())
        })
    });
    assert!(
        saw_have,
        "incremental v2 fetch offered NO `have` line (no negotiation):\n{trace}"
    );
}

// ---------------------------------------------------------------------------
// 3. Empty / unborn-HEAD remote: v2 ls-refs returns no refs, fetch is a no-op,
//    and the call returns (does not hang) — finish_send must close the socket.
// ---------------------------------------------------------------------------
#[test]
fn v2_unborn_remote_is_empty_and_does_not_hang() {
    let Some(srv) = start_server_with(|_work, source| {
        // A bare repo with an unborn HEAD (no commits, no refs).
        std::fs::create_dir_all(source).unwrap();
        git(source, &["init", "-q", "--bare", "."]);
    }) else {
        eprintln!("SKIP: `git daemon` is unavailable");
        return;
    };

    let tmp = tempfile::tempdir().unwrap();
    let local = tmp.path().join("local");
    std::fs::create_dir_all(&local).unwrap();
    git(&local, &["init", "-q", "-b", "main", "."]);
    let local_git = local.join(".git");

    let transport = GitDaemonTransport::new();
    let opts_v2 = ConnectOptions {
        protocol_version: 2,
        ..Default::default()
    };
    let mut conn = match transport.connect(&srv.url, Service::UploadPack, &opts_v2) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("SKIP: could not connect: {e}");
            return;
        }
    };
    assert_eq!(conn.protocol_version(), 2);

    let mut opts = FetchOptions {
        refspecs: vec!["+refs/heads/*:refs/remotes/origin/*".to_owned()],
        tags: TagMode::All,
        ..Default::default()
    };

    // This must return PROMPTLY. The v2 server runs a persistent serve_loop; if
    // `finish_send` regressed (write side never closed), an ls-refs-only fetch
    // with no wants would block forever on the next read. `Box<dyn Connection>`
    // is not `Send`, so we can't run the fetch on a worker thread; instead, arm a
    // watchdog thread that force-kills the daemon after a generous timeout. A
    // hung fetch then sees its socket peer die and returns an error (which we
    // surface), turning a would-be infinite hang into a clean test failure.
    let watchdog_pid = srv.daemon_pid();
    let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let done_w = std::sync::Arc::clone(&done);
    let watchdog = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            if done_w.load(std::sync::atomic::Ordering::SeqCst) {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        // Timed out: kill the daemon so a hung read unblocks.
        kill_pid(watchdog_pid);
    });

    let outcome =
        fetch_remote(&local_git, &mut *conn, &mut opts, &mut NoProgress).expect("v2 empty fetch");
    done.store(true, std::sync::atomic::Ordering::SeqCst);
    let _ = watchdog.join();
    drop(conn);

    assert!(
        outcome.updates.is_empty(),
        "unborn remote should yield no ref updates, got {:?}",
        outcome.updates
    );

    // ls-refs ran (proving v2), even with zero refs to return.
    let trace = read_trace_settled(&srv.trace_path);
    assert!(
        trace.contains("version 2") && trace.contains("command=ls-refs"),
        "v2 ls-refs did not run against unborn remote:\n{trace}"
    );
}

// ---------------------------------------------------------------------------
// 4. Tag fetching over v2: TagMode::All pulls every tag; TagMode::Following
//    pulls only tags reachable from the fetched heads.
// ---------------------------------------------------------------------------
#[test]
fn v2_tag_modes_all_and_following() {
    let Some(srv) = start_server_with(|work, source| {
        build_source(work);
        // An extra tag on an UNREACHABLE commit (a side branch we will NOT fetch)
        // so `Following` must drop it while `All` keeps it.
        git(work, &["checkout", "-q", "-b", "side"]);
        git(work, &["commit", "-q", "--allow-empty", "-m", "side1"]);
        git(work, &["tag", "-a", "vside", "-m", "side tag"]);
        git(work, &["checkout", "-q", "main"]);
        git(
            work,
            &["clone", "-q", "--bare", ".", source.to_str().unwrap()],
        );
        git(source, &["symbolic-ref", "HEAD", "refs/heads/main"]);
    }) else {
        eprintln!("SKIP: `git daemon` is unavailable");
        return;
    };

    let v1_oid = rev_parse(&srv.source, "refs/tags/v1");
    let vside_oid = rev_parse(&srv.source, "refs/tags/vside");
    let transport = GitDaemonTransport::new();
    let opts_v2 = ConnectOptions {
        protocol_version: 2,
        ..Default::default()
    };

    // --- TagMode::All: both tags land. ---
    {
        let tmp = tempfile::tempdir().unwrap();
        let local = tmp.path().join("all");
        std::fs::create_dir_all(&local).unwrap();
        git(&local, &["init", "-q", "-b", "main", "."]);
        let local_git = local.join(".git");

        let mut conn = match transport.connect(&srv.url, Service::UploadPack, &opts_v2) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("SKIP: could not connect: {e}");
                return;
            }
        };
        assert_eq!(conn.protocol_version(), 2);
        let mut opts = FetchOptions {
            // Only fetch main; tags come via TagMode.
            refspecs: vec!["+refs/heads/main:refs/remotes/origin/main".to_owned()],
            tags: TagMode::All,
            ..Default::default()
        };
        fetch_remote(&local_git, &mut *conn, &mut opts, &mut NoProgress).expect("v2 tags=all");
        drop(conn);

        assert_eq!(
            resolve_ref(&local_git, "refs/tags/v1").expect("v1 tag (All)"),
            v1_oid
        );
        assert_eq!(
            resolve_ref(&local_git, "refs/tags/vside").expect("vside tag (All)"),
            vside_oid,
            "TagMode::All must fetch even unreachable-from-main tags"
        );
        fsck_clean(&local);
    }

    // --- TagMode::Following: only v1 (reachable from main) lands; vside dropped. ---
    {
        let tmp = tempfile::tempdir().unwrap();
        let local = tmp.path().join("follow");
        std::fs::create_dir_all(&local).unwrap();
        git(&local, &["init", "-q", "-b", "main", "."]);
        let local_git = local.join(".git");

        let mut conn = transport
            .connect(&srv.url, Service::UploadPack, &opts_v2)
            .expect("connect follow");
        assert_eq!(conn.protocol_version(), 2);
        let mut opts = FetchOptions {
            refspecs: vec!["+refs/heads/main:refs/remotes/origin/main".to_owned()],
            tags: TagMode::Following,
            ..Default::default()
        };
        fetch_remote(&local_git, &mut *conn, &mut opts, &mut NoProgress)
            .expect("v2 tags=following");
        drop(conn);

        // v1 points at main's tip's history -> kept.
        assert_eq!(
            resolve_ref(&local_git, "refs/tags/v1").expect("v1 tag (Following)"),
            v1_oid,
            "TagMode::Following must keep tags reachable from fetched heads"
        );
        // vside points at the `side` branch we did NOT fetch -> dropped.
        assert!(
            resolve_ref(&local_git, "refs/tags/vside").is_err(),
            "TagMode::Following must NOT write a tag unreachable from fetched heads"
        );
        fsck_clean(&local);
    }
}

// ---------------------------------------------------------------------------
// 5. ref-prefix filtering: fetching only refs/heads/* must NOT request or
//    advertise refs/tags/* via ls-refs.
// ---------------------------------------------------------------------------
#[test]
fn v2_ref_prefix_filtering_excludes_tags() {
    let Some(srv) = start_server_with(|work, source| {
        build_source(work);
        git(
            work,
            &["clone", "-q", "--bare", ".", source.to_str().unwrap()],
        );
        git(source, &["symbolic-ref", "HEAD", "refs/heads/main"]);
    }) else {
        eprintln!("SKIP: `git daemon` is unavailable");
        return;
    };

    let tmp = tempfile::tempdir().unwrap();
    let local = tmp.path().join("local");
    std::fs::create_dir_all(&local).unwrap();
    git(&local, &["init", "-q", "-b", "main", "."]);
    let local_git = local.join(".git");

    let transport = GitDaemonTransport::new();
    let opts_v2 = ConnectOptions {
        protocol_version: 2,
        ..Default::default()
    };
    let mut conn = match transport.connect(&srv.url, Service::UploadPack, &opts_v2) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("SKIP: could not connect: {e}");
            return;
        }
    };
    assert_eq!(conn.protocol_version(), 2);

    // Heads only, NO tag following -> ls-refs must only ask for HEAD + refs/heads/.
    let mut opts = FetchOptions {
        refspecs: vec!["+refs/heads/*:refs/remotes/origin/*".to_owned()],
        tags: TagMode::None,
        ..Default::default()
    };
    let outcome = fetch_remote(&local_git, &mut *conn, &mut opts, &mut NoProgress)
        .expect("v2 heads-only fetch");
    drop(conn);

    // No tag ref was written.
    assert!(
        resolve_ref(&local_git, "refs/tags/v1").is_err(),
        "heads-only refspec with TagMode::None must not fetch tags"
    );
    // The outcome's updates are all heads, never tags.
    for u in &outcome.updates {
        assert!(
            !u.remote_ref.starts_with("refs/tags/"),
            "unexpected tag update with heads-only refspec: {}",
            u.remote_ref
        );
    }

    let trace = read_trace_settled(&srv.trace_path);
    assert_trace_is_v2_not_v0(&trace);
    // The client's ls-refs request must NOT contain `ref-prefix refs/tags/`.
    let requested_tag_prefix = trace.lines().any(|l| l.contains("ref-prefix refs/tags/"));
    assert!(
        !requested_tag_prefix,
        "heads-only fetch requested `ref-prefix refs/tags/` (over-broad ls-refs):\n{trace}"
    );
    // And the server therefore never advertised any `refs/tags/` line in the
    // ls-refs response.
    let advertised_tag = trace.lines().any(|l| {
        // Server->client ls-refs line carrying a tag ref.
        l.contains(" refs/tags/") && !l.contains("ref-prefix")
    });
    assert!(
        !advertised_tag,
        "ls-refs advertised a refs/tags/ ref despite heads-only prefix:\n{trace}"
    );
}

// ---------------------------------------------------------------------------
// 6. SHA-256 over v2: the object-format echo must agree with a sha256 server, so
//    a v2 fetch from a sha256 repo lands 64-hex oids and fsck's clean. A broken
//    object-format echo would make upload-pack reject the request (hash
//    mismatch) — this test would then fail to fetch.
// ---------------------------------------------------------------------------
#[test]
fn v2_sha256_fetch_lands_refs_and_objects() {
    // Confirm this git supports the sha256 object format before bothering.
    let probe_dir = tempfile::tempdir().expect("probe tempdir");
    let probe = Command::new("git")
        .args(["init", "--object-format=sha256", "--bare", "."])
        .current_dir(probe_dir.path())
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output();
    if !probe.map(|o| o.status.success()).unwrap_or(false) {
        eprintln!("SKIP: git lacks --object-format=sha256 support");
        return;
    }

    let Some(srv) = start_server_with(|work, source| {
        // Build the SOURCE as a sha256 repo.
        git(
            work,
            &["init", "-q", "--object-format=sha256", "-b", "main", "."],
        );
        std::fs::write(work.join("a.txt"), "one\n").unwrap();
        git(work, &["add", "a.txt"]);
        git(work, &["commit", "-q", "-m", "c1"]);
        std::fs::write(work.join("b.txt"), "two\n").unwrap();
        git(work, &["add", "b.txt"]);
        git(work, &["commit", "-q", "-m", "c2"]);
        git(work, &["tag", "-a", "v1", "-m", "release one"]);
        git(work, &["branch", "topic"]);
        git(
            work,
            &["clone", "-q", "--bare", ".", source.to_str().unwrap()],
        );
        git(source, &["symbolic-ref", "HEAD", "refs/heads/main"]);
    }) else {
        eprintln!("SKIP: `git daemon` is unavailable");
        return;
    };

    // The server is sha256: confirm its oids are 64-hex.
    let main_oid = rev_parse(&srv.source, "refs/heads/main");
    let topic_oid = rev_parse(&srv.source, "refs/heads/topic");
    let tag_oid = rev_parse(&srv.source, "refs/tags/v1");
    assert_eq!(
        main_oid.to_hex().len(),
        64,
        "test setup: expected a sha256 (64-hex) oid, got {}",
        main_oid.to_hex()
    );

    let tmp = tempfile::tempdir().unwrap();
    let local = tmp.path().join("local");
    std::fs::create_dir_all(&local).unwrap();
    // The LOCAL repo must also be sha256 so its odb hash_algo() matches.
    git(
        &local,
        &["init", "-q", "--object-format=sha256", "-b", "main", "."],
    );
    let local_git = local.join(".git");

    let transport = GitDaemonTransport::new();
    let opts_v2 = ConnectOptions {
        protocol_version: 2,
        ..Default::default()
    };
    let mut conn = match transport.connect(&srv.url, Service::UploadPack, &opts_v2) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("SKIP: could not connect: {e}");
            return;
        }
    };
    assert_eq!(conn.protocol_version(), 2, "expected v2 over sha256");
    // The server's v2 capability block must advertise object-format=sha256, which
    // our request echoes back; otherwise the fetch would be a hash mismatch.
    assert!(
        conn.capabilities()
            .iter()
            .any(|c| c == "object-format=sha256"),
        "sha256 server must advertise object-format=sha256; got {:?}",
        conn.capabilities()
    );

    let mut opts = FetchOptions {
        refspecs: vec!["+refs/heads/*:refs/remotes/origin/*".to_owned()],
        tags: TagMode::All,
        ..Default::default()
    };
    let outcome = fetch_remote(&local_git, &mut *conn, &mut opts, &mut NoProgress)
        .expect("v2 sha256 fetch_remote");
    drop(conn);

    // Tracking refs + tag landed with the source's 64-hex oids.
    assert_eq!(
        resolve_ref(&local_git, "refs/remotes/origin/main").unwrap(),
        main_oid
    );
    assert_eq!(
        resolve_ref(&local_git, "refs/remotes/origin/topic").unwrap(),
        topic_oid
    );
    assert_eq!(
        resolve_ref(&local_git, "refs/tags/v1").expect("sha256 tag v1"),
        tag_oid
    );
    assert_eq!(outcome.default_branch.as_deref(), Some("main"));

    // Cross-check every fetched oid against system `git rev-parse` on the source.
    assert_eq!(
        resolve_ref(&local_git, "refs/remotes/origin/main")
            .unwrap()
            .to_hex(),
        git(&srv.source, &["rev-parse", "refs/heads/main"]).trim()
    );

    // fsck clean (the local repo is sha256, so the pack indexes under sha256).
    fsck_clean(&local);

    let trace = read_trace_settled(&srv.trace_path);
    assert_trace_is_v2_not_v0(&trace);
}
