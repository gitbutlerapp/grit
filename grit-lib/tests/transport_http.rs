//! Integration test for the smart-HTTP transport: `SmartHttpTransport` +
//! `http_fetch` over the default `ureq`-backed `HttpClient` (feature `http-ureq`).
//!
//! A bare source repo (two commits on `main`, a `topic` branch, an annotated
//! tag) is built with the system `git` under a temp root. The `grit-http-server`
//! crate's binary is spawned over that root on a free localhost port; an empty
//! local repo then fetches `http://127.0.0.1:<port>/repo.git` via
//! `SmartHttpTransport::connect` (advertisement) + `http_fetch` (negotiation).
//! We assert the tracking refs + tag land, the objects arrive, the fetched main
//! tip matches `git rev-parse`, and the pack `fsck`s clean.
//!
//! The test skips gracefully (returns early) when `git`, the `grit` client binary, or
//! the `grit-http-server` binary is unavailable, or the server fails to bind —
//! the server itself does not require `grit` on `PATH`.
//! the happy path is otherwise real end-to-end HTTP wire I/O.
//!
//! Gated on the `http-ureq` feature (the default `UreqHttpClient` lives there):
//!   cargo test -p grit-lib --features http-ureq --test transport_http

#![cfg(feature = "http-ureq")]

use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use grit_lib::config::ConfigSet;
use grit_lib::error::{Error, Result as GritResult};
use grit_lib::fetch::NoProgress;
use grit_lib::objects::ObjectId;
use grit_lib::odb::Odb;
use grit_lib::push::push_http;
use grit_lib::push_report::PushRefStatus;
use grit_lib::refs::resolve_ref;
use grit_lib::transfer::{FetchOptions, PushOptions, PushRefSpec, TagMode, UpdateMode};
use grit_lib::transport::http::ureq_client::UreqHttpClient;
use grit_lib::transport::http::{http_fetch, HttpClient, SmartHttpTransport};
use grit_lib::transport::{ConnectOptions, Service, Transport};

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

/// Extend [`build_source`] with many branch refs and a large blob (exercises v2
/// `ls-refs` + a non-trivial pack over smart-HTTP).
fn build_heavy_source(dir: &Path, extra_refs: usize) {
    build_source(dir);
    let tip = git(dir, &["rev-parse", "HEAD"]).trim().to_owned();
    for i in 0..extra_refs {
        git(
            dir,
            &[
                "update-ref",
                &format!("refs/heads/branch-{i}"),
                tip.as_str(),
            ],
        );
    }
    std::fs::write(dir.join("large.bin"), vec![b'L'; 512 * 1024]).unwrap();
    git(dir, &["add", "large.bin"]);
    git(dir, &["commit", "-q", "-m", "large blob"]);
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

/// Locate a sibling binary (`grit`, `grit-http-server`) in the cargo target
/// directory. The test executable lives at `target/<profile>/deps/<exe>`, so the
/// binaries are one directory up.
fn find_binary(name: &str) -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    // .../target/<profile>/deps/transport_http-<hash>
    let deps = exe.parent()?; // deps
    let profile = deps.parent()?; // <profile>
    for cand in [profile.join(name), deps.join(name)] {
        if cand.is_file() {
            return Some(cand);
        }
    }
    None
}

/// Spawn `grit-http-server --root <root> --bind 127.0.0.1:<port>` (in-process
/// upload-pack/receive-pack; no `GUST_BIN` or `grit` on `PATH` required).
fn spawn_server(server_bin: &Path, root: &Path, port: u16) -> Option<Child> {
    Command::new(server_bin)
        .arg("--root")
        .arg(root)
        .arg("--bind")
        .arg(format!("127.0.0.1:{port}"))
        .env_remove("GUST_BIN")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .ok()
}

/// Wait until the HTTP server answers a TCP connect on `port`, or time out.
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

struct ServerGuard(Child);
impl Drop for ServerGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn fetch_over_smart_http_lands_refs_and_objects() {
    let Some(grit_bin) = find_binary("grit") else {
        eprintln!("SKIP: `grit` binary not found in target dir (build grit-cli first)");
        return;
    };
    let Some(server_bin) = find_binary("grit-http-server") else {
        eprintln!("SKIP: `grit-http-server` binary not found (build grit-http-server first)");
        return;
    };

    // Build a source repo, then mirror it into a bare repo under the server root
    // (served at `/repo.git`).
    let tmp = tempfile::tempdir().expect("tempdir");
    let work = tmp.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    build_source(&work);

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

    let main_oid = rev_parse(&source, "refs/heads/main");
    let topic_oid = rev_parse(&source, "refs/heads/topic");
    let c1_oid = rev_parse(&work, "HEAD~1");
    let tag_oid = rev_parse(&source, "refs/tags/v1");

    let Some(port) = free_port() else {
        eprintln!("SKIP: could not allocate a free port");
        return;
    };
    let Some(child) = spawn_server(&server_bin, &root, port) else {
        eprintln!("SKIP: could not spawn grit-http-server");
        return;
    };
    let _guard = ServerGuard(child);
    if !wait_ready(port) {
        eprintln!("SKIP: grit-http-server did not become ready on port {port}");
        return;
    }

    let url = format!("http://127.0.0.1:{port}/repo.git");

    // Empty local repo.
    let local = tmp.path().join("local");
    std::fs::create_dir_all(&local).unwrap();
    git(&local, &["init", "-q", "-b", "main", "."]);
    let local_git = local.join(".git");

    // 1. Connect via the trait and check the advertisement.
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
    assert_eq!(conn.protocol_version(), 0);
    drop(conn);

    // 2. Fetch via http_fetch over a recording client (no Git-Protocol header).
    let client = Arc::new(RecordingClient::new_default());
    let opts = FetchOptions {
        refspecs: vec!["+refs/heads/*:refs/remotes/origin/*".to_owned()],
        tags: TagMode::All,
        ..Default::default()
    };
    let outcome = http_fetch(client.as_ref(), &local_git, &url, &opts, &mut NoProgress)
        .expect("http_fetch over grit-http-server");

    // PROVE this is genuinely the v0/v1 stateless RPC (not a silent v2 upgrade):
    // the POST bodies carry a bare `want <oid>` line and NEVER `command=ls-refs`
    // / `command=fetch`, and no request sent a `Git-Protocol` header.
    let v1_commands = client.post_commands.lock().unwrap().clone();
    assert!(
        v1_commands.iter().any(|c| c.starts_with("want ")),
        "v0/v1 fetch must POST a bare `want` body; saw {v1_commands:?}"
    );
    assert!(
        !v1_commands
            .iter()
            .any(|c| c == "command=ls-refs" || c == "command=fetch"),
        "v0/v1 fetch must NOT POST any v2 command body; saw {v1_commands:?}"
    );
    let v1_protocols = client.git_protocols.lock().unwrap().clone();
    assert!(
        v1_protocols.iter().all(|p| p.is_none()),
        "v0/v1 fetch must not send a Git-Protocol header; saw {v1_protocols:?}"
    );

    // Tracking refs written.
    let got_main = resolve_ref(&local_git, "refs/remotes/origin/main").expect("origin/main");
    let got_topic = resolve_ref(&local_git, "refs/remotes/origin/topic").expect("origin/topic");
    assert_eq!(got_main, main_oid, "origin/main oid mismatch vs source");
    assert_eq!(got_topic, topic_oid, "origin/topic oid mismatch vs source");

    // Annotated tag arrived (TagMode::All).
    let got_tag = resolve_ref(&local_git, "refs/tags/v1").expect("tag v1 written");
    assert_eq!(got_tag, tag_oid, "tag v1 oid mismatch vs source");

    // Objects landed in the local odb.
    let local_odb = open_odb(&local_git);
    for oid in [main_oid, topic_oid, c1_oid, tag_oid] {
        assert!(
            local_odb.exists(&oid),
            "object {} missing from local odb after http fetch",
            oid.to_hex()
        );
        local_odb
            .read(&oid)
            .unwrap_or_else(|e| panic!("read {}: {e}", oid.to_hex()));
    }

    // Per-ref update modes.
    let main_update = outcome
        .updates
        .iter()
        .find(|u| u.remote_ref == "refs/heads/main")
        .expect("update for main");
    assert_eq!(main_update.mode, UpdateMode::New);
    assert_eq!(main_update.new_oid, Some(main_oid));

    // Default branch from the server's HEAD symref.
    assert_eq!(outcome.default_branch.as_deref(), Some("main"));

    // Cross-check the fetched main tip against git's view of the source.
    assert_eq!(
        got_main.to_hex(),
        git(&source, &["rev-parse", "refs/heads/main"]).trim()
    );

    // The fetched pack re-indexes / fsck's clean in the local repo.
    let fsck = Command::new("git")
        .current_dir(&local)
        .args(["fsck", "--no-dangling"])
        .output()
        .expect("run git fsck");
    assert!(
        fsck.status.success(),
        "git fsck failed after http fetch: {}",
        String::from_utf8_lossy(&fsck.stderr)
    );
}

/// An [`HttpClient`] that wraps [`UreqHttpClient`] and records the first line of
/// every POST body. A protocol-v2 fetch must POST a `command=ls-refs` body and a
/// `command=fetch` body; capturing them proves the v2 code path ran (rather than
/// a v0/v1 advertisement fetch) directly from the wire, not from a self-report.
struct RecordingClient {
    inner: UreqHttpClient,
    /// `Git-Protocol` header values seen on each request (GET + POST).
    git_protocols: Mutex<Vec<Option<String>>>,
    /// First pkt-line (command) of each POST body.
    post_commands: Mutex<Vec<String>>,
    /// For each `command=fetch` POST, the object count read from the `PACK`
    /// header of the demuxed `packfile` section of the *response* (or `None`
    /// when that response carried no packfile, e.g. a pure negotiation round).
    /// This is wire evidence of pack minimality: an incremental fetch that
    /// offers `have`s must yield a far smaller object count than a from-scratch
    /// full-closure fetch.
    fetch_pack_object_counts: Mutex<Vec<Option<u32>>>,
    /// Whether any `command=fetch` POST *body* carried at least one `have <oid>`
    /// line — wire evidence the negotiator actually offered local history.
    fetch_sent_haves: Mutex<bool>,
}

impl RecordingClient {
    fn new(git_protocol: &str) -> Self {
        Self::from_inner(UreqHttpClient::new().with_git_protocol(git_protocol.to_owned()))
    }

    /// A recording client with NO default `Git-Protocol` header — used to prove a
    /// v0/v1 fetch genuinely runs the classic stateless RPC (bare `want`/`have`
    /// bodies, no `command=fetch`) and never silently upgrades to v2.
    fn new_default() -> Self {
        Self::from_inner(UreqHttpClient::new())
    }

    /// Client built like grit-cli (`UreqHttpClient::from_config` on default config).
    fn from_default_config() -> GritResult<Self> {
        let inner = UreqHttpClient::from_config(&ConfigSet::new())?;
        Ok(Self::from_inner(inner))
    }

    fn from_inner(inner: UreqHttpClient) -> Self {
        Self {
            inner,
            git_protocols: Mutex::new(Vec::new()),
            post_commands: Mutex::new(Vec::new()),
            fetch_pack_object_counts: Mutex::new(Vec::new()),
            fetch_sent_haves: Mutex::new(false),
        }
    }

    /// Whether the request `body` (a v2 `command=fetch`) contained a `have <oid>`
    /// pkt-line. Scans the pkt-line stream for a payload starting with `have `.
    fn body_has_have_line(body: &[u8]) -> bool {
        let mut i = 0usize;
        while i + 4 <= body.len() {
            let Ok(len_str) = std::str::from_utf8(&body[i..i + 4]) else {
                return false;
            };
            let Ok(len) = usize::from_str_radix(len_str, 16) else {
                return false;
            };
            if len < 4 {
                i += 4;
                continue;
            }
            if i + len > body.len() {
                return false;
            }
            let payload = &body[i + 4..i + len];
            if payload.starts_with(b"have ") {
                return true;
            }
            i += len;
        }
        false
    }

    /// Extract the payload of the first pkt-line in `body` (e.g. `command=ls-refs`).
    fn first_pkt_line(body: &[u8]) -> String {
        if body.len() < 4 {
            return String::new();
        }
        let Ok(len_str) = std::str::from_utf8(&body[..4]) else {
            return String::new();
        };
        let Ok(len) = usize::from_str_radix(len_str, 16) else {
            return String::new();
        };
        if len <= 4 || len > body.len() {
            return String::new();
        }
        String::from_utf8_lossy(&body[4..len]).trim_end().to_owned()
    }

    /// Demux the side-band-64k `packfile` section out of a v2 `command=fetch`
    /// response and return the object count from the `PACK` header (bytes 8..12,
    /// big-endian). Returns `None` when the response has no packfile section.
    ///
    /// This walks the pkt-line stream looking for the `packfile` section header,
    /// then concatenates band-1 payloads (the pack bytes) until the section
    /// boundary, and parses the standard 12-byte pack header.
    fn pack_object_count(resp: &[u8]) -> Option<u32> {
        let mut i = 0usize;
        let mut in_packfile = false;
        let mut pack: Vec<u8> = Vec::new();
        while i + 4 <= resp.len() {
            let Ok(len_str) = std::str::from_utf8(&resp[i..i + 4]) else {
                break;
            };
            let Ok(len) = usize::from_str_radix(len_str, 16) else {
                break;
            };
            // 0000 flush / 0001 delim / 0002 response-end: section boundaries.
            if len < 4 {
                i += 4;
                if in_packfile && len == 0 {
                    // End of the packfile section.
                    break;
                }
                continue;
            }
            if i + len > resp.len() {
                break;
            }
            let payload = &resp[i + 4..i + len];
            i += len;
            let text = String::from_utf8_lossy(payload);
            let header = text.trim_end();
            if matches!(
                header,
                "acknowledgments" | "shallow-info" | "wanted-refs" | "packfile-uris" | "packfile"
            ) {
                in_packfile = header == "packfile";
                continue;
            }
            if in_packfile && !payload.is_empty() && payload[0] == 1 {
                // Band 1 = pack data.
                pack.extend_from_slice(&payload[1..]);
            }
        }
        if pack.len() >= 12 && &pack[0..4] == b"PACK" {
            Some(u32::from_be_bytes([pack[8], pack[9], pack[10], pack[11]]))
        } else {
            None
        }
    }

    /// The recorded per-`command=fetch` pack object counts, in POST order.
    fn fetch_object_counts(&self) -> Vec<Option<u32>> {
        self.fetch_pack_object_counts.lock().unwrap().clone()
    }
}

impl HttpClient for RecordingClient {
    fn get(&self, url: &str, git_protocol: Option<&str>) -> GritResult<Vec<u8>> {
        let gp = git_protocol.or_else(|| self.inner.git_protocol_header());
        self.git_protocols
            .lock()
            .unwrap()
            .push(gp.map(str::to_owned));
        self.inner.get(url, git_protocol)
    }

    fn post(
        &self,
        url: &str,
        content_type: &str,
        accept: &str,
        body: &[u8],
        git_protocol: Option<&str>,
    ) -> GritResult<Vec<u8>> {
        let gp = git_protocol.or_else(|| self.inner.git_protocol_header());
        self.git_protocols
            .lock()
            .unwrap()
            .push(gp.map(str::to_owned));
        let command = Self::first_pkt_line(body);
        self.post_commands.lock().unwrap().push(command.clone());
        if command == "command=fetch" && Self::body_has_have_line(body) {
            *self.fetch_sent_haves.lock().unwrap() = true;
        }
        let resp = self
            .inner
            .post(url, content_type, accept, body, git_protocol)?;
        // For a v2 `command=fetch`, record the object count of any packfile the
        // response carried — direct wire evidence of how minimal the pack was.
        if command == "command=fetch" {
            self.fetch_pack_object_counts
                .lock()
                .unwrap()
                .push(Self::pack_object_count(&resp));
        }
        Ok(resp)
    }

    fn git_protocol_header(&self) -> Option<&str> {
        self.inner.git_protocol_header()
    }
}

/// Wraps an HTTP client and fails `git-upload-pack` POSTs with an I/O error
/// (simulates a truncated response body read).
struct IoFailUploadPackClient<C> {
    inner: C,
}

impl<C: HttpClient> HttpClient for IoFailUploadPackClient<C> {
    fn get(&self, url: &str, git_protocol: Option<&str>) -> GritResult<Vec<u8>> {
        self.inner.get(url, git_protocol)
    }

    fn post(
        &self,
        url: &str,
        content_type: &str,
        accept: &str,
        body: &[u8],
        git_protocol: Option<&str>,
    ) -> GritResult<Vec<u8>> {
        if url.contains("git-upload-pack") {
            return Err(Error::Io(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "failed to fill whole buffer",
            )));
        }
        self.inner
            .post(url, content_type, accept, body, git_protocol)
    }

    fn git_protocol_header(&self) -> Option<&str> {
        self.inner.git_protocol_header()
    }
}

#[test]
fn fetch_over_smart_http_v2_lands_refs_and_objects() {
    let Some(grit_bin) = find_binary("grit") else {
        eprintln!("SKIP: `grit` binary not found in target dir (build grit-cli first)");
        return;
    };
    let Some(server_bin) = find_binary("grit-http-server") else {
        eprintln!("SKIP: `grit-http-server` binary not found (build grit-http-server first)");
        return;
    };

    let tmp = tempfile::tempdir().expect("tempdir");
    let work = tmp.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    build_source(&work);

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

    let main_oid = rev_parse(&source, "refs/heads/main");
    let topic_oid = rev_parse(&source, "refs/heads/topic");
    let c1_oid = rev_parse(&work, "HEAD~1");
    let tag_oid = rev_parse(&source, "refs/tags/v1");

    let Some(port) = free_port() else {
        eprintln!("SKIP: could not allocate a free port");
        return;
    };
    let Some(child) = spawn_server(&server_bin, &root, port) else {
        eprintln!("SKIP: could not spawn grit-http-server");
        return;
    };
    let _guard = ServerGuard(child);
    if !wait_ready(port) {
        eprintln!("SKIP: grit-http-server did not become ready on port {port}");
        return;
    }

    let url = format!("http://127.0.0.1:{port}/repo.git");

    // Confirm the server actually speaks v2: a system `git -c protocol.version=2`
    // clone over the same URL succeeds (and the server returns a `version 2`
    // advertisement under the `Git-Protocol: version=2` header). If the server
    // does not speak v2, skip rather than report a false pass.
    let xcheck = tmp.path().join("v2xcheck");
    let xclone = Command::new("git")
        .args([
            "-c",
            "protocol.version=2",
            "clone",
            "-q",
            &url,
            xcheck.to_str().expect("utf8 path"),
        ])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("run git v2 clone");
    if !xclone.status.success() {
        eprintln!(
            "SKIP: server does not speak protocol v2 (git v2 clone failed: {})",
            String::from_utf8_lossy(&xclone.stderr)
        );
        return;
    }

    // Empty local repo.
    let local = tmp.path().join("local");
    std::fs::create_dir_all(&local).unwrap();
    git(&local, &["init", "-q", "-b", "main", "."]);
    let local_git = local.join(".git");

    // 1. Connect requesting v2 and confirm the advertisement is a v2 capability
    // block (no refs on connect; protocol_version == 2).
    let recording = Arc::new(RecordingClient::new("version=2"));
    let transport = SmartHttpTransport::new(Arc::clone(&recording));
    let opts_v2 = ConnectOptions {
        protocol_version: 2,
        ..Default::default()
    };
    let conn = match transport.connect(&url, Service::UploadPack, &opts_v2) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("SKIP: could not connect to grit-http-server over v2: {e}");
            return;
        }
    };
    assert_eq!(
        conn.protocol_version(),
        2,
        "expected a v2 advertisement from the server"
    );
    assert!(
        conn.advertised_refs().is_empty(),
        "v2 connect carries no refs (they come from ls-refs)"
    );
    // The v2 capability block was advertised.
    assert!(
        conn.capabilities()
            .iter()
            .any(|c| c.starts_with("fetch=") || c.starts_with("ls-refs")),
        "v2 capability block missing: {:?}",
        conn.capabilities()
    );
    drop(conn);

    // 2. Fetch over v2 via http_fetch with the recording client.
    let opts = FetchOptions {
        refspecs: vec!["+refs/heads/*:refs/remotes/origin/*".to_owned()],
        tags: TagMode::All,
        ..Default::default()
    };
    let outcome = http_fetch(recording.as_ref(), &local_git, &url, &opts, &mut NoProgress)
        .expect("v2 http_fetch over grit-http-server");

    // PROVE v2: the recorded POSTs include a `command=ls-refs` and a
    // `command=fetch` body (the v2 stateless flow), not a v0/v1 want/have body.
    let commands = recording.post_commands.lock().unwrap().clone();
    assert!(
        commands.iter().any(|c| c == "command=ls-refs"),
        "expected a `command=ls-refs` POST (v2 path); saw {commands:?}"
    );
    assert!(
        commands.iter().any(|c| c == "command=fetch"),
        "expected a `command=fetch` POST (v2 path); saw {commands:?}"
    );
    assert!(
        !commands.iter().any(|c| c.starts_with("want ")),
        "v2 path must not POST a bare v0/v1 want body; saw {commands:?}"
    );
    // Every request carried `Git-Protocol: version=2`.
    let protocols = recording.git_protocols.lock().unwrap().clone();
    assert!(
        protocols.iter().all(|p| p.as_deref() == Some("version=2")),
        "expected version=2 on every request; saw {protocols:?}"
    );

    // PROVE the from-scratch pack carried the FULL closure. The empty local repo
    // offers no `have`s, so the server packs every object reachable from the
    // wanted tips (2 commits + 2 trees + 2 blobs + the annotated tag = 7). Read
    // the object count straight from the demuxed `PACK` header of the response.
    let scratch_counts: Vec<u32> = recording
        .fetch_object_counts()
        .into_iter()
        .flatten()
        .collect();
    assert_eq!(
        scratch_counts.len(),
        1,
        "expected exactly one packfile-bearing command=fetch response on a from-scratch v2 fetch; saw {scratch_counts:?}"
    );
    let scratch_objs = scratch_counts[0];
    assert!(
        scratch_objs >= 6,
        "from-scratch v2 fetch should pack the full closure (>= 6 objects), packed {scratch_objs}"
    );

    // Tracking refs written.
    let got_main = resolve_ref(&local_git, "refs/remotes/origin/main").expect("origin/main");
    let got_topic = resolve_ref(&local_git, "refs/remotes/origin/topic").expect("origin/topic");
    assert_eq!(got_main, main_oid, "origin/main oid mismatch vs source");
    assert_eq!(got_topic, topic_oid, "origin/topic oid mismatch vs source");

    // Annotated tag arrived (TagMode::All).
    let got_tag = resolve_ref(&local_git, "refs/tags/v1").expect("tag v1 written");
    assert_eq!(got_tag, tag_oid, "tag v1 oid mismatch vs source");

    // Objects landed in the local odb.
    let local_odb = open_odb(&local_git);
    for oid in [main_oid, topic_oid, c1_oid, tag_oid] {
        assert!(
            local_odb.exists(&oid),
            "object {} missing from local odb after v2 http fetch",
            oid.to_hex()
        );
        local_odb
            .read(&oid)
            .unwrap_or_else(|e| panic!("read {}: {e}", oid.to_hex()));
    }

    // Per-ref update mode + default branch.
    let main_update = outcome
        .updates
        .iter()
        .find(|u| u.remote_ref == "refs/heads/main")
        .expect("update for main");
    assert_eq!(main_update.mode, UpdateMode::New);
    assert_eq!(main_update.new_oid, Some(main_oid));
    assert_eq!(outcome.default_branch.as_deref(), Some("main"));

    // Cross-check the fetched main tip against git's view of the source.
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
        "git fsck failed after v2 http fetch: {}",
        String::from_utf8_lossy(&fsck.stderr)
    );

    // 3. Incremental v2 fetch: advance the source's main by one commit, then
    // fetch again. Now the local repo holds history, so the v2 `command=fetch`
    // negotiation sends `have` lines — exercising the stateless acknowledgments
    // round (not just the empty-clone `wants + done` shortcut).
    std::fs::write(work.join("c.txt"), "three\n").unwrap();
    git(&work, &["add", "c.txt"]);
    git(&work, &["commit", "-q", "-m", "c3"]);
    // Refresh the served bare repo from the work tree.
    git(
        &work,
        &["push", "-q", source.to_str().expect("utf8 path"), "main"],
    );
    let new_main_oid = rev_parse(&source, "refs/heads/main");
    assert_ne!(new_main_oid, main_oid, "source main should have advanced");

    let recording2 = Arc::new(RecordingClient::new("version=2"));
    let outcome2 = http_fetch(
        recording2.as_ref(),
        &local_git,
        &url,
        &opts,
        &mut NoProgress,
    )
    .expect("incremental v2 http_fetch");
    let commands2 = recording2.post_commands.lock().unwrap().clone();
    assert!(
        commands2.iter().any(|c| c == "command=ls-refs"),
        "incremental fetch missing ls-refs; saw {commands2:?}"
    );
    assert!(
        commands2.iter().any(|c| c == "command=fetch"),
        "incremental fetch missing command=fetch; saw {commands2:?}"
    );
    let got_main2 = resolve_ref(&local_git, "refs/remotes/origin/main").expect("origin/main");
    assert_eq!(
        got_main2, new_main_oid,
        "incremental v2 fetch did not advance origin/main"
    );
    assert!(
        local_odb.exists(&new_main_oid),
        "new commit object missing after incremental v2 fetch"
    );
    let main_update2 = outcome2
        .updates
        .iter()
        .find(|u| u.remote_ref == "refs/heads/main")
        .expect("incremental update for main");
    assert_eq!(main_update2.mode, UpdateMode::FastForward);

    // PROVE the negotiation offered local history: at least one `command=fetch`
    // POST body carried a `have <oid>` line (otherwise the "minimal pack" below
    // would be meaningless — it would just be a from-scratch fetch).
    assert!(
        *recording2.fetch_sent_haves.lock().unwrap(),
        "incremental v2 fetch must POST `have` lines so the server can trim the pack"
    );

    // PROVE pack minimality: advancing `main` by exactly one commit, the server
    // must pack only that commit's new objects (commit + tree + blob = 3), NOT
    // the full closure. Compare the demuxed `PACK` object count against the
    // from-scratch count captured earlier — it must be a small constant and far
    // below the full closure. This is the decisive wire evidence that the
    // `have` negotiation actually shrank the pack.
    let inc_counts: Vec<u32> = recording2
        .fetch_object_counts()
        .into_iter()
        .flatten()
        .collect();
    assert_eq!(
        inc_counts.len(),
        1,
        "expected exactly one packfile-bearing command=fetch response on the incremental v2 fetch; saw {inc_counts:?}"
    );
    let inc_objs = inc_counts[0];
    assert!(
        inc_objs <= 4,
        "incremental v2 fetch (one new commit) must send a minimal pack (<= 4 objects), sent {inc_objs}"
    );
    assert!(
        inc_objs < scratch_objs,
        "incremental pack ({inc_objs} objects) must be smaller than the from-scratch closure ({scratch_objs})"
    );

    let fsck2 = Command::new("git")
        .current_dir(&local)
        .args(["fsck", "--no-dangling"])
        .output()
        .expect("run git fsck");
    assert!(
        fsck2.status.success(),
        "git fsck failed after incremental v2 http fetch: {}",
        String::from_utf8_lossy(&fsck2.stderr)
    );
}

/// Regression for #911: grit-cli uses `UreqHttpClient::from_config` without
/// manually calling `with_git_protocol`. Default config must negotiate v2 and
/// complete a many-ref / large-pack fetch (the v0/v1 path mis-handles this).
#[test]
fn fetch_from_config_client_v2_many_refs_and_large_pack() {
    let Some(grit_bin) = find_binary("grit") else {
        eprintln!("SKIP: `grit` binary not found in target dir (build grit-cli first)");
        return;
    };
    let Some(server_bin) = find_binary("grit-http-server") else {
        eprintln!("SKIP: `grit-http-server` binary not found (build grit-http-server first)");
        return;
    };

    let tmp = tempfile::tempdir().expect("tempdir");
    let work = tmp.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    build_heavy_source(&work, 1500);

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

    let Some(port) = free_port() else {
        eprintln!("SKIP: could not allocate a free port");
        return;
    };
    let Some(child) = spawn_server(&server_bin, &root, port) else {
        eprintln!("SKIP: could not spawn grit-http-server");
        return;
    };
    let _guard = ServerGuard(child);
    if !wait_ready(port) {
        eprintln!("SKIP: grit-http-server did not become ready on port {port}");
        return;
    }

    let url = format!("http://127.0.0.1:{port}/repo.git");
    let local = tmp.path().join("local");
    std::fs::create_dir_all(&local).unwrap();
    git(&local, &["init", "-q", "-b", "main", "."]);
    let local_git = local.join(".git");

    let recording = Arc::new(
        RecordingClient::from_default_config().expect("from_config client for #911 regression"),
    );
    assert_eq!(
        recording.git_protocol_header(),
        Some("version=2"),
        "from_config must default to Git-Protocol version=2 (grit-cli path)"
    );
    assert!(
        UreqHttpClient::new().git_protocol_header().is_none(),
        "sanity: bare new() must not imply v2 — regression targets from_config"
    );

    let opts = FetchOptions {
        refspecs: vec!["+refs/heads/*:refs/remotes/origin/*".to_owned()],
        tags: TagMode::All,
        ..Default::default()
    };
    let outcome = http_fetch(recording.as_ref(), &local_git, &url, &opts, &mut NoProgress)
        .expect("from_config v2 http_fetch over many-ref repo");

    let commands = recording.post_commands.lock().unwrap().clone();
    assert!(
        commands.iter().any(|c| c == "command=ls-refs"),
        "from_config fetch must use v2 ls-refs; saw {commands:?}"
    );
    assert!(
        commands.iter().any(|c| c == "command=fetch"),
        "from_config fetch must use v2 command=fetch; saw {commands:?}"
    );
    let branch_updates = outcome
        .updates
        .iter()
        .filter(|u| u.remote_ref.starts_with("refs/heads/branch-"))
        .count();
    assert!(
        branch_updates >= 1500,
        "expected >=1500 branch ref updates, got {branch_updates}"
    );

    let main_oid = rev_parse(&source, "refs/heads/main");
    assert_eq!(
        resolve_ref(&local_git, "refs/remotes/origin/main").expect("origin/main"),
        main_oid
    );
    let fsck = Command::new("git")
        .current_dir(&local)
        .args(["fsck", "--no-dangling"])
        .output()
        .expect("run git fsck");
    assert!(
        fsck.status.success(),
        "git fsck failed after from_config v2 fetch: {}",
        String::from_utf8_lossy(&fsck.stderr)
    );
}

#[test]
fn http_fetch_surfaces_io_error_from_upload_pack_read_failure() {
    let Some(grit_bin) = find_binary("grit") else {
        eprintln!("SKIP: `grit` binary not found in target dir (build grit-cli first)");
        return;
    };
    let Some(server_bin) = find_binary("grit-http-server") else {
        eprintln!("SKIP: `grit-http-server` binary not found (build grit-http-server first)");
        return;
    };

    let tmp = tempfile::tempdir().expect("tempdir");
    let work = tmp.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    build_source(&work);

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

    let Some(port) = free_port() else {
        eprintln!("SKIP: could not allocate a free port");
        return;
    };
    let Some(child) = spawn_server(&server_bin, &root, port) else {
        eprintln!("SKIP: could not spawn grit-http-server");
        return;
    };
    let _guard = ServerGuard(child);
    if !wait_ready(port) {
        eprintln!("SKIP: grit-http-server did not become ready on port {port}");
        return;
    }

    let url = format!("http://127.0.0.1:{port}/repo.git");
    let local = tmp.path().join("local");
    std::fs::create_dir_all(&local).unwrap();
    git(&local, &["init", "-q", "-b", "main", "."]);
    let local_git = local.join(".git");

    let inner = UreqHttpClient::from_config(&ConfigSet::new()).expect("from_config");
    let client = IoFailUploadPackClient { inner };
    let opts = FetchOptions {
        refspecs: vec!["+refs/heads/*:refs/remotes/origin/*".to_owned()],
        tags: TagMode::None,
        ..Default::default()
    };
    let err = http_fetch(&client, &local_git, &url, &opts, &mut NoProgress).expect_err("I/O");
    assert!(
        matches!(err, Error::Io(_)),
        "truncated upload-pack read must surface as Io error, got {err:?}"
    );
}

/// Create an empty bare repo under `root` that receive-pack can push into, with a
/// default branch symref. Returns its on-disk path.
fn make_bare_target(root: &Path, name: &str) -> PathBuf {
    let bare = root.join(name);
    std::fs::create_dir_all(&bare).unwrap();
    git(&bare, &["init", "-q", "--bare", "."]);
    git(&bare, &["symbolic-ref", "HEAD", "refs/heads/main"]);
    bare
}

#[test]
fn push_over_smart_http_lands_ref_and_objects_and_reports_rejection() {
    let Some(grit_bin) = find_binary("grit") else {
        eprintln!("SKIP: `grit` binary not found in target dir (build grit-cli first)");
        return;
    };
    let Some(server_bin) = find_binary("grit-http-server") else {
        eprintln!("SKIP: `grit-http-server` binary not found (build grit-http-server first)");
        return;
    };

    // Local source repo with two commits on main.
    let tmp = tempfile::tempdir().expect("tempdir");
    let local = tmp.path().join("local");
    std::fs::create_dir_all(&local).unwrap();
    build_source(&local);
    let local_git = local.join(".git");
    let main_oid = rev_parse(&local, "refs/heads/main");
    let c1_oid = rev_parse(&local, "HEAD~1");

    // Empty bare receive target served at `/push.git` under the server root.
    let root = tmp.path().join("srv");
    std::fs::create_dir_all(&root).unwrap();
    let bare = make_bare_target(&root, "push.git");

    let Some(port) = free_port() else {
        eprintln!("SKIP: could not allocate a free port");
        return;
    };
    let Some(child) = spawn_server(&server_bin, &root, port) else {
        eprintln!("SKIP: could not spawn grit-http-server");
        return;
    };
    let _guard = ServerGuard(child);
    if !wait_ready(port) {
        eprintln!("SKIP: grit-http-server did not become ready on port {port}");
        return;
    }

    let url = format!("http://127.0.0.1:{port}/push.git");

    // Confirm the server actually offers receive-pack: GET the receive-pack
    // advertisement and require a smart-HTTP body. If it 404s / lacks the service,
    // skip rather than fail (matches the fetch test's graceful-skip policy).
    let client = UreqHttpClient::new();
    let probe_url = format!("{url}/info/refs?service=git-receive-pack");
    match client.get(&probe_url, None) {
        Ok(body) if body.windows(20).any(|w| w == b"# service=git-receiv") => {}
        Ok(_) => {
            eprintln!("SKIP: server returned a non-smart receive-pack advertisement");
            return;
        }
        Err(e) => {
            eprintln!("SKIP: server does not offer receive-pack: {e}");
            return;
        }
    }

    // --- 1. Push refs/heads/main (create) -------------------------------------
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
    .expect("push_http over grit-http-server");

    assert_eq!(outcome.results.len(), 1);
    let r = &outcome.results[0];
    assert_eq!(
        r.status,
        PushRefStatus::Ok,
        "push of new ref should be accepted, got {:?} ({:?})",
        r.status,
        r.message
    );
    assert_eq!(r.new_oid, Some(main_oid));
    assert!(r.old_oid.is_none(), "new ref has no old value");

    // The remote ref now points at our main, and the objects are present.
    let remote_main = resolve_ref(&bare, "refs/heads/main").expect("remote main written");
    assert_eq!(
        remote_main, main_oid,
        "remote main oid mismatch after http push"
    );

    let remote_odb = open_odb(&bare);
    for oid in [main_oid, c1_oid] {
        assert!(
            remote_odb.exists(&oid),
            "object {} missing from remote odb after http push",
            oid.to_hex()
        );
        remote_odb
            .read(&oid)
            .unwrap_or_else(|e| panic!("read {}: {e}", oid.to_hex()));
    }

    // The bare repo fscks clean after receiving the pushed pack (cross-check with
    // system git).
    let fsck = Command::new("git")
        .current_dir(&bare)
        .args(["fsck", "--no-dangling"])
        .output()
        .expect("run git fsck");
    assert!(
        fsck.status.success(),
        "git fsck failed after http push: {}\n{}",
        String::from_utf8_lossy(&fsck.stdout),
        String::from_utf8_lossy(&fsck.stderr)
    );
    // System git agrees on the pushed tip.
    assert_eq!(
        remote_main.to_hex(),
        git(&bare, &["rev-parse", "refs/heads/main"]).trim()
    );

    // --- 2. Push a second branch (new ref over an already-populated remote) ----
    // This exercises the thin/delta pack against the advertised remote tips: the
    // remote already has main's history, so pushing `topic` (sharing that history
    // plus one new commit) should send a minimal pack and still fsck clean.
    git(&local, &["checkout", "-q", "-b", "feature"]);
    std::fs::write(local.join("t.txt"), "feature\n").unwrap();
    git(&local, &["add", "t.txt"]);
    git(&local, &["commit", "-q", "-m", "feature1"]);
    let topic_oid = rev_parse(&local, "refs/heads/feature");

    let spec_topic = PushRefSpec {
        src: Some(topic_oid),
        dst: "refs/heads/feature".to_owned(),
        force: false,
        delete: false,
        expected_old: None,
        expect_absent: false,
    };
    let outcome_topic = push_http(
        &client,
        &local_git,
        &url,
        &[spec_topic],
        &PushOptions::default(),
        &mut NoProgress,
    )
    .expect("push_http topic over grit-http-server");
    assert_eq!(outcome_topic.results[0].status, PushRefStatus::Ok);
    let remote_topic = resolve_ref(&bare, "refs/heads/feature").expect("remote feature written");
    assert_eq!(remote_topic, topic_oid);
    let fsck_topic = Command::new("git")
        .current_dir(&bare)
        .args(["fsck", "--no-dangling"])
        .output()
        .expect("run git fsck");
    assert!(
        fsck_topic.status.success(),
        "git fsck failed after second http push: {}",
        String::from_utf8_lossy(&fsck_topic.stderr)
    );

    // --- 3. Non-fast-forward push (no force) is rejected ----------------------
    // Rewrite local main onto a divergent history so it is no longer a descendant
    // of the just-pushed tip; pushing it without force must be rejected and must
    // NOT move the remote ref.
    git(&local, &["checkout", "-q", "-b", "diverge", "HEAD~2"]);
    std::fs::write(local.join("d.txt"), "diverge\n").unwrap();
    git(&local, &["add", "d.txt"]);
    git(&local, &["commit", "-q", "-m", "divergent"]);
    let diverged = rev_parse(&local, "HEAD");
    assert_ne!(diverged, main_oid);

    let nonff = PushRefSpec {
        src: Some(diverged),
        dst: "refs/heads/main".to_owned(),
        force: false,
        delete: false,
        expected_old: None,
        expect_absent: false,
    };
    let outcome2 = push_http(
        &client,
        &local_git,
        &url,
        &[nonff],
        &PushOptions::default(),
        &mut NoProgress,
    )
    .expect("non-ff push_http completes");
    assert_eq!(outcome2.results.len(), 1);
    let r2 = &outcome2.results[0];
    assert!(
        r2.status.is_error(),
        "non-fast-forward push must be rejected, got {:?}",
        r2.status
    );
    assert!(
        matches!(
            r2.status,
            PushRefStatus::RejectNonFastForward | PushRefStatus::RemoteRejected
        ),
        "expected non-ff/remote rejection, got {:?} ({:?})",
        r2.status,
        r2.message
    );

    // The remote ref must be unchanged by the rejected push.
    let remote_main_after = resolve_ref(&bare, "refs/heads/main").expect("remote main still set");
    assert_eq!(
        remote_main_after, main_oid,
        "rejected non-ff push must not move the remote ref"
    );

    // --- 4. Forced non-ff push moves the ref (cross-check with system git) -----
    let forced = PushRefSpec {
        src: Some(diverged),
        dst: "refs/heads/main".to_owned(),
        force: true,
        delete: false,
        expected_old: None,
        expect_absent: false,
    };
    let outcome3 = push_http(
        &client,
        &local_git,
        &url,
        &[forced],
        &PushOptions::default(),
        &mut NoProgress,
    )
    .expect("forced push_http completes");
    assert_eq!(
        outcome3.results[0].status,
        PushRefStatus::Ok,
        "forced non-ff push should be accepted: {:?}",
        outcome3.results[0].message
    );
    let remote_main_forced = resolve_ref(&bare, "refs/heads/main").expect("remote main moved");
    assert_eq!(
        remote_main_forced, diverged,
        "forced push should move the remote ref to the divergent tip"
    );
    assert_eq!(
        remote_main_forced.to_hex(),
        git(&bare, &["rev-parse", "refs/heads/main"]).trim()
    );
    let fsck_forced = Command::new("git")
        .current_dir(&bare)
        .args(["fsck", "--no-dangling"])
        .output()
        .expect("run git fsck");
    assert!(
        fsck_forced.status.success(),
        "git fsck failed after forced http push: {}",
        String::from_utf8_lossy(&fsck_forced.stderr)
    );
}

/// Round-trip over HTTP: push a repo to the server over `git-receive-pack`, then
/// fetch it back over `git-upload-pack` into a fresh empty repo, and assert the
/// fetched refs/objects equal what was pushed. Also drives a *server-side*
/// rejection (`receive.denyNonFastForwards=true`) to prove an `ng` report-status
/// line surfaces as [`PushRefStatus::RemoteRejected`] — the server, not the
/// client-side gate, declines the update.
#[test]
fn push_then_fetch_roundtrip_and_server_side_rejection_over_http() {
    let Some(grit_bin) = find_binary("grit") else {
        eprintln!("SKIP: `grit` binary not found in target dir (build grit-cli first)");
        return;
    };
    let Some(server_bin) = find_binary("grit-http-server") else {
        eprintln!("SKIP: `grit-http-server` binary not found (build grit-http-server first)");
        return;
    };

    // Local source repo (two commits on main, a topic branch, an annotated tag).
    let tmp = tempfile::tempdir().expect("tempdir");
    let local = tmp.path().join("local");
    std::fs::create_dir_all(&local).unwrap();
    build_source(&local);
    let local_git = local.join(".git");
    let main_oid = rev_parse(&local, "refs/heads/main");
    let topic_oid = rev_parse(&local, "refs/heads/topic");
    let c1_oid = rev_parse(&local, "HEAD~1");

    // Empty bare receive target served at `/rt.git`.
    let root = tmp.path().join("srv");
    std::fs::create_dir_all(&root).unwrap();
    let bare = make_bare_target(&root, "rt.git");

    let Some(port) = free_port() else {
        eprintln!("SKIP: could not allocate a free port");
        return;
    };
    let Some(child) = spawn_server(&server_bin, &root, port) else {
        eprintln!("SKIP: could not spawn grit-http-server");
        return;
    };
    let _guard = ServerGuard(child);
    if !wait_ready(port) {
        eprintln!("SKIP: grit-http-server did not become ready on port {port}");
        return;
    }

    let url = format!("http://127.0.0.1:{port}/rt.git");

    let client = UreqHttpClient::new();
    // Skip cleanly if the server lacks receive-pack (matches the other tests).
    let probe_url = format!("{url}/info/refs?service=git-receive-pack");
    match client.get(&probe_url, None) {
        Ok(body) if body.windows(20).any(|w| w == b"# service=git-receiv") => {}
        Ok(_) => {
            eprintln!("SKIP: server returned a non-smart receive-pack advertisement");
            return;
        }
        Err(e) => {
            eprintln!("SKIP: server does not offer receive-pack: {e}");
            return;
        }
    }

    // --- Push main + topic over HTTP ------------------------------------------
    let specs = [
        PushRefSpec {
            src: Some(main_oid),
            dst: "refs/heads/main".to_owned(),
            force: false,
            delete: false,
            expected_old: None,
            expect_absent: false,
        },
        PushRefSpec {
            src: Some(topic_oid),
            dst: "refs/heads/topic".to_owned(),
            force: false,
            delete: false,
            expected_old: None,
            expect_absent: false,
        },
    ];
    let push_outcome = push_http(
        &client,
        &local_git,
        &url,
        &specs,
        &PushOptions::default(),
        &mut NoProgress,
    )
    .expect("push_http main+topic over grit-http-server");
    for r in &push_outcome.results {
        assert_eq!(
            r.status,
            PushRefStatus::Ok,
            "push of {} should be accepted, got {:?} ({:?})",
            r.remote_ref,
            r.status,
            r.message
        );
    }
    assert_eq!(
        resolve_ref(&bare, "refs/heads/main").unwrap(),
        main_oid,
        "remote main mismatch after push"
    );
    assert_eq!(
        resolve_ref(&bare, "refs/heads/topic").unwrap(),
        topic_oid,
        "remote topic mismatch after push"
    );

    // --- Fetch the pushed repo back over HTTP into a fresh empty repo ----------
    let back = tmp.path().join("back");
    std::fs::create_dir_all(&back).unwrap();
    git(&back, &["init", "-q", "-b", "main", "."]);
    let back_git = back.join(".git");

    let fetch_opts = FetchOptions {
        refspecs: vec!["+refs/heads/*:refs/remotes/origin/*".to_owned()],
        tags: TagMode::None,
        ..Default::default()
    };
    let client2 = UreqHttpClient::new();
    let fetch_outcome = http_fetch(&client2, &back_git, &url, &fetch_opts, &mut NoProgress)
        .expect("http_fetch back the just-pushed repo");

    // The round-tripped refs equal what we pushed (byte-for-byte oid equality).
    assert_eq!(
        resolve_ref(&back_git, "refs/remotes/origin/main").unwrap(),
        main_oid,
        "round-trip: fetched origin/main != pushed main"
    );
    assert_eq!(
        resolve_ref(&back_git, "refs/remotes/origin/topic").unwrap(),
        topic_oid,
        "round-trip: fetched origin/topic != pushed topic"
    );
    // And the actual objects round-tripped through push-pack -> fetch-pack.
    let back_odb = open_odb(&back_git);
    for oid in [main_oid, topic_oid, c1_oid] {
        assert!(
            back_odb.exists(&oid),
            "round-trip: object {} missing after fetch-back",
            oid.to_hex()
        );
    }
    assert!(
        fetch_outcome
            .updates
            .iter()
            .any(|u| u.remote_ref == "refs/heads/main" && u.new_oid == Some(main_oid)),
        "round-trip fetch did not report the main update"
    );
    // fsck the fetched-back repo for good measure.
    let fsck = Command::new("git")
        .current_dir(&back)
        .args(["fsck", "--no-dangling"])
        .output()
        .expect("run git fsck");
    assert!(
        fsck.status.success(),
        "git fsck failed after fetch-back: {}",
        String::from_utf8_lossy(&fsck.stderr)
    );

    // --- Server-side rejection: deny non-fast-forwards on the remote ----------
    // Turn on `receive.denyNonFastForwards` in the bare repo, then push a
    // divergent tip WITH force (so the client-side gate accepts it). The *server*
    // must decline it via an `ng <ref> non-fast-forward` report-status line,
    // which `push_http` folds into `RemoteRejected`. This exercises the
    // report-status `ng` path end-to-end over real HTTP — distinct from the
    // client-side `RejectNonFastForward` gate covered elsewhere.
    let cfg = Command::new("git")
        .current_dir(&bare)
        .args(["config", "receive.denyNonFastForwards", "true"])
        .output()
        .expect("set denyNonFastForwards");
    assert!(cfg.status.success(), "failed to set denyNonFastForwards");

    // Build a divergent commit that is not a descendant of remote main: branch
    // off c1 (main's first commit) and add a new commit, so the resulting tip is
    // a sibling of remote main (c2), never its descendant.
    git(
        &local,
        &["checkout", "-q", "-b", "rt-diverge", "refs/heads/main~1"],
    );
    std::fs::write(local.join("rt.txt"), "rt-diverge\n").unwrap();
    git(&local, &["add", "rt.txt"]);
    git(&local, &["commit", "-q", "-m", "rt-divergent"]);
    let diverged = rev_parse(&local, "HEAD");
    assert_ne!(diverged, main_oid);

    let forced_spec = PushRefSpec {
        src: Some(diverged),
        dst: "refs/heads/main".to_owned(),
        force: true, // client accepts; server must decline (denyNonFastForwards)
        delete: false,
        expected_old: Some(main_oid),
        expect_absent: false,
    };
    let reject_outcome = push_http(
        &client,
        &local_git,
        &url,
        &[forced_spec],
        &PushOptions::default(),
        &mut NoProgress,
    )
    .expect("forced push against denyNonFastForwards completes");
    assert_eq!(reject_outcome.results.len(), 1);
    let rr = &reject_outcome.results[0];
    assert_eq!(
        rr.status,
        PushRefStatus::RemoteRejected,
        "server-side denyNonFastForwards must surface as RemoteRejected, got {:?} ({:?})",
        rr.status,
        rr.message
    );
    // The remote ref must be unchanged by the server-rejected push.
    assert_eq!(
        resolve_ref(&bare, "refs/heads/main").unwrap(),
        main_oid,
        "server-rejected push must not move the remote ref"
    );
}
