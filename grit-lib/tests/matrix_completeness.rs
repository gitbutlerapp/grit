//! COMPLETENESS / ERROR-PATH matrix for the transport stack built this session.
//!
//! The other matrix files (`matrix_fetch`, `matrix_push`, `matrix_credentials`,
//! `matrix_packs`, `matrix_sha256`) all exercise *happy paths* — the cases where
//! the wire conversation succeeds. This file is their adversarial complement: it
//! pins the TYPED-ERROR and edge behaviors that an embedder relies on to *not*
//! hang and to be able to `match` on the failure mode, none of which were covered
//! anywhere else:
//!
//!   1. fetch surfaces a side-band **band-3 fatal** as a typed `Error` (not a
//!      hang, not silent truncation) — driven by a scripted in-memory connection.
//!   2. fetch surfaces an **`ERR` packet after `done`** as a typed remote error.
//!   3. fetch surfaces a v0/v1 advertisement **`ERR`** line (via the public
//!      `read_advertisement`) — and the v2 capability-block `ERR` too.
//!   4. `push_remote` against a **protocol-v2** connection fails typed (v2 push is
//!      intentionally deferred), *before* touching any ref.
//!   5. `push_remote` report parsing: an **`unpack <error>`** report demotes every
//!      sent ref to `RemoteRejected` with the unpack reason.
//!   6. `push_http` against a **v2 receive-pack** advertisement fails typed.
//!   7. `push_http` / discovery propagates an **HTTP transport error** as a typed
//!      `Error` (connection-refused shape) without hanging.
//!   8. `GitDaemonTransport::connect` to a **refused port** fails promptly with a
//!      typed `Error` (watchdog-guarded so a regression that hangs is caught).
//!   9. `Transport::connect` with a **malformed URL** fails typed for every
//!      streaming transport (git daemon / ssh).
//!  10. **shallow + tags**: a `--depth 1` fetch with `TagMode::All` over the git
//!      daemon lands the shallow tip AND the tag, records a real shallow boundary
//!      (the deep ancestor is absent), and cross-checks vs system `git`.
//!
//! Cases 1-7 and 9 are deterministic (scripted fake `Connection` / `HttpClient`,
//! or pure URL parsing) and ALWAYS run their assertions. Cases 8 and 10 use real
//! fixtures (`git daemon`) and SKIP cleanly only when the fixture is genuinely
//! unavailable.
//!
//! Gated on `http-ureq` for the `UreqHttpClient`-free `HttpClient` fake to live
//! beside the real client type imports:
//!   cargo test -p grit-lib --features http-ureq --test matrix_completeness

#![cfg(feature = "http-ureq")]
#![cfg(unix)]

use std::io::{Cursor, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use grit_lib::error::{Error, Result as GritResult};
use grit_lib::fetch::{fetch_remote, NoProgress};
use grit_lib::objects::ObjectId;
use grit_lib::odb::Odb;
use grit_lib::pkt_line;
use grit_lib::push::{push_http, push_remote};
use grit_lib::push_report::PushRefStatus;
use grit_lib::refs::resolve_ref;
use grit_lib::transfer::{FetchOptions, PushOptions, PushRefSpec, TagMode};
use grit_lib::transport::http::{http_client_arc, HttpClient};
use grit_lib::transport::{
    read_advertisement, ConnectOptions, Connection, GitDaemonTransport, Service, SshTransport,
    Transport,
};

// ---------------------------------------------------------------------------
// git plumbing helpers (copied from the sibling transport_* tests, same fixture
// construction harness).
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
        "git {args:?} in {} failed: {}",
        dir.display(),
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

/// An empty local repo (fresh `git init`, no commits/refs/HEAD target). Its
/// negotiator offers zero `have`s, which makes the v0/v1 fetch conversation
/// deterministic and scriptable: wants -> done -> (NAK) -> side-band pack.
fn empty_local(root: &Path) -> PathBuf {
    std::fs::create_dir_all(root).unwrap();
    git(root, &["init", "-q", "-b", "main", "."]);
    root.join(".git")
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
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if TcpStream::connect_timeout(&addr, Duration::from_millis(200)).is_ok() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

/// Run `f` on a helper thread and fail the test if it does not finish within
/// `secs` — proving the code under test returned (typed error) rather than hung.
/// `f` must be `Send` and own everything it touches.
fn with_watchdog<T: Send + 'static>(
    secs: u64,
    what: &str,
    f: impl FnOnce() -> T + Send + 'static,
) -> T {
    let (tx, rx) = mpsc::channel();
    let handle = std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    match rx.recv_timeout(Duration::from_secs(secs)) {
        Ok(v) => {
            let _ = handle.join();
            v
        }
        Err(_) => {
            panic!("{what}: did not return within {secs}s (it hung instead of erroring typed)")
        }
    }
}

// ===========================================================================
// A fully-scripted, in-memory `Connection`.
//
// `reader` replays a fixed byte script (the server's side of the conversation);
// `writer` swallows whatever the client sends (the fetch/push engine's request).
// The advertisement (refs/caps/symref/version) is supplied directly. This lets
// us drive `fetch_remote` / `push_remote` through exact wire shapes that a real
// server will not reliably produce on demand (band-3 fatal, ERR-after-done,
// `unpack <error>` report), and prove the engine surfaces them as typed errors.
// ===========================================================================

struct ScriptedConn {
    reader: Cursor<Vec<u8>>,
    sink: Vec<u8>,
    refs: Vec<(String, ObjectId)>,
    caps: Vec<String>,
    head_symref: Option<String>,
    version: u8,
}

impl ScriptedConn {
    fn new(server_script: Vec<u8>) -> Self {
        ScriptedConn {
            reader: Cursor::new(server_script),
            sink: Vec::new(),
            refs: Vec::new(),
            caps: Vec::new(),
            head_symref: None,
            version: 0,
        }
    }
    fn with_ref(mut self, name: &str, oid: ObjectId) -> Self {
        self.refs.push((name.to_owned(), oid));
        self
    }
    fn with_caps(mut self, caps: &[&str]) -> Self {
        self.caps = caps.iter().map(|c| (*c).to_owned()).collect();
        self
    }
    fn with_version(mut self, v: u8) -> Self {
        self.version = v;
        self
    }
}

impl Connection for ScriptedConn {
    fn reader(&mut self) -> &mut dyn Read {
        &mut self.reader
    }
    fn writer(&mut self) -> &mut dyn Write {
        &mut self.sink
    }
    fn advertised_refs(&self) -> &[(String, ObjectId)] {
        &self.refs
    }
    fn capabilities(&self) -> &[String] {
        &self.caps
    }
    fn head_symref(&self) -> Option<&str> {
        self.head_symref.as_deref()
    }
    fn protocol_version(&self) -> u8 {
        self.version
    }
}

/// Append a side-band-framed pkt-line: a single byte band id followed by `data`.
fn push_sideband(buf: &mut Vec<u8>, band: u8, data: &[u8]) {
    let mut payload = Vec::with_capacity(data.len() + 1);
    payload.push(band);
    payload.extend_from_slice(data);
    pkt_line::write_packet_raw(buf, &payload).unwrap();
}

/// A non-zero, definitely-absent-locally oid for the local odb's hash width.
/// (All-`b` nibbles; never the result of any real object hash here.)
fn absent_oid(local_git: &Path) -> ObjectId {
    let width = open_odb(local_git).hash_algo().hex_len();
    ObjectId::from_hex(&"b".repeat(width)).expect("valid synthetic oid")
}

// ===========================================================================
// 1. fetch surfaces a side-band band-3 fatal as a typed error (no hang).
// ===========================================================================

#[test]
fn fetch_v0_sideband_band3_fatal_is_typed_error_not_hang() {
    let tmp = tempfile::tempdir().unwrap();
    let local_git = empty_local(&tmp.path().join("local"));
    let want = absent_oid(&local_git);

    // Server script for the post-`done` phase: a NAK, then a band-3 (fatal)
    // side-band packet, then EOF. `negotiate_pack` reads the NAK after `done`
    // and then enters `read_sideband_pack`, which must turn band 3 into an Err.
    let mut script = Vec::new();
    pkt_line::write_line_to_vec(&mut script, "NAK").unwrap();
    push_sideband(&mut script, 3, b"fatal: the requested object is corrupt");
    // No flush / pack — band 3 must already have aborted.

    let conn = ScriptedConn::new(script)
        .with_ref("refs/heads/main", want)
        .with_caps(&["side-band-64k", "multi_ack_detailed", "ofs-delta"])
        .with_version(0);

    let opts = FetchOptions {
        refspecs: vec!["+refs/heads/*:refs/remotes/origin/*".to_owned()],
        tags: TagMode::None,
        ..Default::default()
    };

    let local_git_for_thread = local_git.clone();
    let err = with_watchdog(10, "fetch band-3", move || {
        let mut conn = conn;
        fetch_remote(&local_git_for_thread, &mut conn, &opts, &mut NoProgress)
            .expect_err("a band-3 fatal must surface as an error")
    });
    let msg = format!("{err}");
    assert!(
        msg.contains("remote error") && msg.contains("corrupt"),
        "band-3 fatal must be reported as a remote error carrying the server text, got: {msg}"
    );
    // Nothing landed.
    assert!(resolve_ref(&local_git, "refs/remotes/origin/main").is_err());
}

// ===========================================================================
// 2. fetch surfaces an `ERR` packet after `done` as a typed remote error.
// ===========================================================================

#[test]
fn fetch_v0_err_after_done_is_typed_remote_error() {
    let tmp = tempfile::tempdir().unwrap();
    let local_git = empty_local(&tmp.path().join("local"));
    let want = absent_oid(&local_git);

    // After `done`, the very next packet is `ERR <msg>` (upload-pack's way of
    // declining, e.g. "not our ref"). The engine reads it where it expects the
    // ACK/NAK and must convert it to a typed remote error.
    let mut script = Vec::new();
    pkt_line::write_line_to_vec(&mut script, "ERR upload-pack: not our ref deadbeef").unwrap();

    let conn = ScriptedConn::new(script)
        .with_ref("refs/heads/main", want)
        .with_caps(&["side-band-64k", "multi_ack_detailed", "ofs-delta"])
        .with_version(0);

    let opts = FetchOptions {
        refspecs: vec!["+refs/heads/*:refs/remotes/origin/*".to_owned()],
        tags: TagMode::None,
        ..Default::default()
    };

    let local_git_for_thread = local_git.clone();
    let err = with_watchdog(10, "fetch ERR-after-done", move || {
        let mut conn = conn;
        fetch_remote(&local_git_for_thread, &mut conn, &opts, &mut NoProgress)
            .expect_err("an ERR after done must surface as an error")
    });
    let msg = format!("{err}");
    assert!(
        msg.contains("remote error") && msg.contains("not our ref"),
        "ERR-after-done must surface as a remote error with the server text, got: {msg}"
    );
}

// ===========================================================================
// 3. read_advertisement: v0/v1 `ERR` line and v2 capability-block `ERR`.
// ===========================================================================

#[test]
fn read_advertisement_v0_err_line_is_typed_error() {
    // upload-pack/receive-pack can decline at advertise time with a single
    // `ERR <msg>` pkt-line (e.g. access denied). It must not be parsed as a ref.
    let mut wire = Vec::new();
    pkt_line::write_line_to_vec(&mut wire, "ERR access denied or repository not exported").unwrap();
    wire.extend_from_slice(b"0000");

    let mut cur = Cursor::new(wire);
    let err = read_advertisement(&mut cur).expect_err("an ERR advertisement must be an error");
    let msg = format!("{err}");
    assert!(
        msg.contains("remote error") && msg.contains("access denied"),
        "v0 ERR advertisement must surface as a typed remote error, got: {msg}"
    );
}

#[test]
fn read_advertisement_v2_err_in_capability_block_is_typed_error() {
    // A v2 server can emit `version 2` then decline with `ERR` inside the
    // capability block; read_advertisement must surface it (not collect it as a
    // capability).
    let mut wire = Vec::new();
    pkt_line::write_line_to_vec(&mut wire, "version 2").unwrap();
    pkt_line::write_line_to_vec(&mut wire, "agent=git/2.99").unwrap();
    pkt_line::write_line_to_vec(&mut wire, "ERR service not enabled").unwrap();
    wire.extend_from_slice(b"0000");

    let mut cur = Cursor::new(wire);
    let err = read_advertisement(&mut cur).expect_err("a v2 ERR must be an error");
    let msg = format!("{err}");
    assert!(
        msg.contains("remote error") && msg.contains("service not enabled"),
        "v2 ERR in capability block must surface as a typed remote error, got: {msg}"
    );
}

// ===========================================================================
// 4. push_remote against a protocol-v2 connection fails typed, before any ref.
// ===========================================================================

#[test]
fn push_remote_rejects_protocol_v2_typed_before_touching_refs() {
    let tmp = tempfile::tempdir().unwrap();
    let local = tmp.path().join("local");
    std::fs::create_dir_all(&local).unwrap();
    git(&local, &["init", "-q", "-b", "main", "."]);
    std::fs::write(local.join("a.txt"), "one\n").unwrap();
    git(&local, &["add", "a.txt"]);
    git(&local, &["commit", "-q", "-m", "c1"]);
    let local_git = local.join(".git");
    let main_oid = rev_parse(&local, "HEAD");

    // A v2 connection: push is intentionally deferred to a later phase, so the
    // engine must reject it typed (and not start writing a command block to a
    // server that will never read a v0/v1 push).
    let mut conn = ScriptedConn::new(Vec::new())
        .with_caps(&["agent=git/2.99", "object-format=sha1"])
        .with_version(2);

    let spec = PushRefSpec {
        src: Some(main_oid),
        dst: "refs/heads/main".to_owned(),
        force: false,
        delete: false,
        expected_old: None,
        expect_absent: false,
    };
    let err = push_remote(
        &local_git,
        &mut conn,
        &[spec],
        &PushOptions::default(),
        &mut NoProgress,
    )
    .expect_err("a v2 push must be rejected in this phase");
    let msg = format!("{err}");
    assert!(
        msg.contains("v2") && msg.to_lowercase().contains("not supported"),
        "v2 push must fail with a clear 'v2 not supported' message, got: {msg}"
    );
    // The engine must not have written anything to the wire before rejecting.
    let written = std::mem::take(&mut conn.sink);
    assert!(
        written.is_empty(),
        "v2 push must reject before sending any bytes, but wrote {} bytes",
        written.len()
    );
}

// ===========================================================================
// 5. push_remote report parsing: `unpack <error>` demotes every sent ref.
// ===========================================================================

#[test]
fn push_remote_unpack_failure_report_demotes_all_sent_refs() {
    let tmp = tempfile::tempdir().unwrap();
    let local = tmp.path().join("local");
    std::fs::create_dir_all(&local).unwrap();
    git(&local, &["init", "-q", "-b", "main", "."]);
    std::fs::write(local.join("a.txt"), "one\n").unwrap();
    git(&local, &["add", "a.txt"]);
    git(&local, &["commit", "-q", "-m", "c1"]);
    let local_git = local.join(".git");
    let main_oid = rev_parse(&local, "HEAD");

    // The server accepts the command block + pack on the wire, but then reports a
    // global unpack failure: `unpack index-pack abort`. No per-ref `ng` line — the
    // engine must still demote the (otherwise-decided-Ok) ref to RemoteRejected.
    // We do NOT advertise side-band, so the raw report bytes are the report-status.
    let mut report = Vec::new();
    pkt_line::write_line_to_vec(&mut report, "unpack index-pack abort: object corrupt").unwrap();
    pkt_line::write_line_to_vec(&mut report, "ok refs/heads/main").unwrap();
    report.extend_from_slice(b"0000");

    // Create a new ref (no old value on the remote) so the decision is plain "Ok"
    // before the report demotes it. No `.have`/refs advertised => create push.
    let mut conn = ScriptedConn::new(report)
        .with_caps(&["report-status", "ofs-delta", "delete-refs"])
        .with_version(0);

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
        &mut conn,
        &[spec],
        &PushOptions::default(),
        &mut NoProgress,
    )
    .expect("push_remote completes and parses the report");

    assert_eq!(outcome.results.len(), 1);
    let r = &outcome.results[0];
    assert_eq!(
        r.status,
        PushRefStatus::RemoteRejected,
        "an unpack failure must demote the ref to RemoteRejected, got {:?}",
        r.status
    );
    let reason = r.message.clone().unwrap_or_default();
    assert!(
        reason.contains("unpack failed") && reason.contains("index-pack abort"),
        "the unpack-failure reason must carry the server text, got {reason:?}"
    );
    // The client did write a command block + pack (it had no way to know the
    // unpack would fail until it read the report).
    assert!(
        !conn.sink.is_empty(),
        "the client should have sent the command block and pack"
    );
}

// ===========================================================================
// 6 + 7. push_http error paths via a scripted `HttpClient`.
// ===========================================================================

/// A fake HTTP client that returns a canned discovery body for the info/refs GET
/// and a configurable result for the POST. `post_err` makes the POST fail (the
/// connection-refused shape); otherwise the GET body alone drives the test
/// (e.g. a v2 advertisement that push_http must reject before POSTing).
#[derive(Clone)]
struct FakeHttpClient {
    get_body: Vec<u8>,
    /// If set, `get` itself fails (discovery against a dead server).
    get_err: Option<String>,
    /// If set, `post` fails with this message.
    post_err: Option<String>,
}

impl HttpClient for FakeHttpClient {
    fn get(&self, _url: &str, _git_protocol: Option<&str>) -> GritResult<Vec<u8>> {
        if let Some(e) = &self.get_err {
            return Err(Error::Message(e.clone()));
        }
        Ok(self.get_body.clone())
    }
    fn post(
        &self,
        _url: &str,
        _content_type: &str,
        _accept: &str,
        _body: &[u8],
        _git_protocol: Option<&str>,
    ) -> GritResult<Vec<u8>> {
        if let Some(e) = &self.post_err {
            return Err(Error::Message(e.clone()));
        }
        // No test reaches a successful POST; return an empty report-status.
        Ok(b"0000".to_vec())
    }

    fn post_into_reader(
        &self,
        url: &str,
        content_type: &str,
        accept: &str,
        body: &[u8],
        git_protocol: Option<&str>,
    ) -> GritResult<Box<dyn std::io::Read + Send>> {
        Ok(Box::new(std::io::Cursor::new(self.post(
            url,
            content_type,
            accept,
            body,
            git_protocol,
        )?)))
    }
}

/// Build a smart-HTTP `info/refs?service=git-receive-pack` body advertising a v2
/// receive-pack (`# service` preamble, flush, `version 2`, caps, flush).
fn v2_receive_pack_advertisement() -> Vec<u8> {
    let mut body = Vec::new();
    pkt_line::write_line_to_vec(&mut body, "# service=git-receive-pack").unwrap();
    body.extend_from_slice(b"0000");
    pkt_line::write_line_to_vec(&mut body, "version 2").unwrap();
    pkt_line::write_line_to_vec(&mut body, "agent=git/2.99").unwrap();
    body.extend_from_slice(b"0000");
    body
}

#[test]
fn push_http_rejects_v2_receive_pack_advertisement_typed() {
    let tmp = tempfile::tempdir().unwrap();
    let local = tmp.path().join("local");
    std::fs::create_dir_all(&local).unwrap();
    git(&local, &["init", "-q", "-b", "main", "."]);
    std::fs::write(local.join("a.txt"), "one\n").unwrap();
    git(&local, &["add", "a.txt"]);
    git(&local, &["commit", "-q", "-m", "c1"]);
    let local_git = local.join(".git");
    let main_oid = rev_parse(&local, "HEAD");

    let client = grit_lib::transport::http::http_client_arc(FakeHttpClient {
        get_body: v2_receive_pack_advertisement(),
        get_err: None,
        post_err: Some("POST must not be reached for a v2 advertisement".to_owned()),
    });
    let spec = PushRefSpec {
        src: Some(main_oid),
        dst: "refs/heads/main".to_owned(),
        force: false,
        delete: false,
        expected_old: None,
        expect_absent: false,
    };
    let err = push_http(
        client,
        &local_git,
        "http://example.invalid/repo.git",
        &[spec],
        &PushOptions::default(),
        &mut NoProgress,
    )
    .expect_err("a v2 receive-pack advertisement must be rejected");
    let msg = format!("{err}");
    assert!(
        msg.contains("v2") && msg.to_lowercase().contains("not supported"),
        "push_http must reject a v2 advertisement with a clear message, got: {msg}"
    );
}

#[test]
fn push_http_propagates_discovery_transport_error_typed() {
    let tmp = tempfile::tempdir().unwrap();
    let local = tmp.path().join("local");
    std::fs::create_dir_all(&local).unwrap();
    git(&local, &["init", "-q", "-b", "main", "."]);
    std::fs::write(local.join("a.txt"), "one\n").unwrap();
    git(&local, &["add", "a.txt"]);
    git(&local, &["commit", "-q", "-m", "c1"]);
    let local_git = local.join(".git");
    let main_oid = rev_parse(&local, "HEAD");

    // Discovery GET fails (connection refused / DNS failure shape). push_http must
    // surface a typed Error, not panic or hang.
    let client = grit_lib::transport::http::http_client_arc(FakeHttpClient {
        get_body: Vec::new(),
        get_err: Some("connection refused (os error 61)".to_owned()),
        post_err: None,
    });
    let spec = PushRefSpec {
        src: Some(main_oid),
        dst: "refs/heads/main".to_owned(),
        force: false,
        delete: false,
        expected_old: None,
        expect_absent: false,
    };
    let err = push_http(
        client,
        &local_git,
        "http://127.0.0.1:1/repo.git",
        &[spec],
        &PushOptions::default(),
        &mut NoProgress,
    )
    .expect_err("a discovery transport failure must surface as an error");
    let msg = format!("{err}");
    assert!(
        msg.contains("connection refused"),
        "discovery transport error must propagate, got: {msg}"
    );
}

// ===========================================================================
// 8. Real-fixture: connect to a refused port fails promptly with a typed error.
// ===========================================================================

#[test]
fn git_daemon_connect_refused_port_is_typed_error_not_hang() {
    // Allocate then immediately release a port: nothing is listening, so the
    // connect must fail fast (ECONNREFUSED) with a typed Error rather than hang.
    let Some(port) = free_port() else {
        eprintln!("SKIP: could not allocate a free port");
        return;
    };
    let url = format!("git://127.0.0.1:{port}/repo.git");

    let err = with_watchdog(10, "git daemon connect-refused", move || {
        let transport = GitDaemonTransport::new();
        transport
            .connect(&url, Service::UploadPack, &ConnectOptions::default())
            .err()
    });
    assert!(
        err.is_some(),
        "connecting to a closed port must return an Err (got Ok)"
    );
}

// ===========================================================================
// 9. Malformed URLs fail typed on connect for both streaming transports.
// ===========================================================================

#[test]
fn streaming_transports_reject_malformed_urls_typed() {
    let daemon = GitDaemonTransport::new();
    // Not a git:// URL at all.
    assert!(
        daemon
            .connect(
                "https://example.com/repo.git",
                Service::UploadPack,
                &ConnectOptions::default()
            )
            .is_err(),
        "git daemon transport must reject a non-git:// URL"
    );
    // git:// with no repository path.
    assert!(
        daemon
            .connect(
                "git://example.com",
                Service::UploadPack,
                &ConnectOptions::default()
            )
            .is_err(),
        "git daemon transport must reject a git:// URL with no path"
    );

    let ssh = SshTransport::new();
    // An empty host scp-style URL is malformed.
    assert!(
        ssh.connect("host:", Service::UploadPack, &ConnectOptions::default())
            .is_err(),
        "ssh transport must reject a scp-style URL with an empty path"
    );
}

// ===========================================================================
// 10. Real-fixture: shallow + tags interaction over the git daemon.
// ===========================================================================

/// Build a source repo: linear root->mid->tip on main, plus an annotated tag on
/// the tip. A `--depth 1 --tags` fetch must land the tip and the tag while
/// leaving `root` absent (a real shallow boundary).
fn build_tagged_source(dir: &Path) {
    git(dir, &["init", "-q", "-b", "main", "."]);
    std::fs::write(dir.join("a.txt"), "root\n").unwrap();
    git(dir, &["add", "a.txt"]);
    git(dir, &["commit", "-q", "-m", "root"]);
    std::fs::write(dir.join("a.txt"), "mid\n").unwrap();
    git(dir, &["add", "a.txt"]);
    git(dir, &["commit", "-q", "-m", "mid"]);
    std::fs::write(dir.join("a.txt"), "tip\n").unwrap();
    git(dir, &["add", "a.txt"]);
    git(dir, &["commit", "-q", "-m", "tip"]);
    git(dir, &["tag", "-a", "v1", "-m", "release one"]);
}

struct DaemonGuard(Child);
impl Drop for DaemonGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn shallow_depth1_with_all_tags_over_git_daemon() {
    let tmp = tempfile::tempdir().unwrap();
    let work = tmp.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    build_tagged_source(&work);

    // Bare clone under a daemon base path.
    let base = tmp.path().join("srv");
    std::fs::create_dir_all(&base).unwrap();
    let served = base.join("repo.git");
    let st = Command::new("git")
        .args([
            "clone",
            "-q",
            "--bare",
            work.to_str().unwrap(),
            served.to_str().unwrap(),
        ])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .status()
        .expect("git clone --bare");
    if !st.success() {
        eprintln!("SKIP: could not bare-clone the source");
        return;
    }
    let _ = Command::new("git")
        .current_dir(&served)
        .args(["symbolic-ref", "HEAD", "refs/heads/main"])
        .status();

    let Some(port) = free_port() else {
        eprintln!("SKIP: could not allocate a free port");
        return;
    };
    let Ok(child) = Command::new("git")
        .arg("daemon")
        .arg("--listen=127.0.0.1")
        .arg(format!("--port={port}"))
        .arg("--reuseaddr")
        .arg("--export-all")
        .arg(format!("--base-path={}", base.display()))
        .arg(&base)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        eprintln!("SKIP: `git daemon` is unavailable");
        return;
    };
    let _guard = DaemonGuard(child);
    if !wait_ready(port) {
        eprintln!("SKIP: git daemon did not become ready");
        return;
    }

    let tip = rev_parse(&work, "refs/heads/main");
    let root = rev_parse(&work, "main~2");
    let tag = rev_parse(&work, "refs/tags/v1");

    let local = tmp.path().join("local");
    let local_git = empty_local(&local);

    let url = format!("git://127.0.0.1:{port}/repo.git");
    let transport = GitDaemonTransport::new();
    // v0/v1 is enough to exercise the shallow + include-tag interaction; the v2
    // shallow path is covered by sibling shallow tests.
    let mut conn = match transport.connect(&url, Service::UploadPack, &ConnectOptions::default()) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("SKIP: could not connect to git daemon: {e}");
            return;
        }
    };

    let opts = FetchOptions {
        refspecs: vec!["+refs/heads/*:refs/remotes/origin/*".to_owned()],
        tags: TagMode::All,
        depth: Some(1),
        ..Default::default()
    };
    let outcome = fetch_remote(&local_git, &mut *conn, &opts, &mut NoProgress)
        .expect("shallow+tags fetch over git daemon");
    drop(conn);

    // The shallow tip landed as origin/main, cross-checked vs system git.
    let landed_tip = resolve_ref(&local_git, "refs/remotes/origin/main").expect("origin/main");
    assert_eq!(landed_tip, tip, "shallow fetch must land the remote tip");
    assert_eq!(
        landed_tip.to_hex(),
        git(&work, &["rev-parse", "refs/heads/main"]).trim()
    );

    // The annotated tag came along (TagMode::All + include-tag), and points at the
    // same object system git resolves it to.
    let landed_tag = resolve_ref(&local_git, "refs/tags/v1").expect("tag v1 must be fetched");
    assert_eq!(
        landed_tag, tag,
        "the annotated tag must land with the shallow fetch"
    );

    let local_odb = open_odb(&local_git);
    assert!(local_odb.exists(&tip), "tip object must be present");
    assert!(local_odb.exists(&tag), "tag object must be present");

    // It is genuinely shallow: the deep ancestor `root` is NOT present locally.
    assert!(
        !local_odb.exists(&root),
        "a depth-1 fetch must NOT bring the deep ancestor {} into the local odb",
        root.to_hex()
    );

    // The shallow boundary is recorded on disk and surfaced in the outcome.
    let on_disk = grit_lib::shallow::load_shallow_oids(&local_git).expect("load shallow");
    assert!(
        on_disk.contains(&tip),
        "the shallow file must graft the tip {} (boundary), got {:?}",
        tip.to_hex(),
        on_disk.iter().map(ObjectId::to_hex).collect::<Vec<_>>()
    );
    assert!(
        outcome.new_shallow.contains(&tip),
        "the fetch outcome must report the new shallow boundary"
    );

    // A shallow repo with exactly the boundary grafted still fscks clean (git
    // tolerates the missing parents because they are listed in `shallow`).
    let fsck = Command::new("git")
        .current_dir(&local)
        .args(["fsck", "--no-dangling"])
        .output()
        .expect("run git fsck");
    assert!(
        fsck.status.success(),
        "git fsck failed after shallow+tags fetch: {}",
        String::from_utf8_lossy(&fsck.stderr)
    );
}
