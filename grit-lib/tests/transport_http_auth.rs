//! Integration test for HTTP basic-auth on the smart-HTTP transport.
//!
//! Spawns `grit-http-server --require-auth user:pass` (it returns `401` +
//! `WWW-Authenticate: Basic realm="git"` unless the request carries the matching
//! `Authorization: Basic` header) over a bare source repo, then drives
//! `http_fetch` / `push_http` through the default `ureq`-backed client wired with
//! a [`CredentialProvider`]:
//!
//!   * with the RIGHT credentials → fetch/push succeed (refs + objects land, the
//!     pack `fsck`s clean, the tracking tip matches `git rev-parse`);
//!   * with WRONG / NO credentials → the call fails fast with the typed
//!     [`grit_lib::error::Error::Auth`] (never a hang);
//!   * the provider's `approve` / `reject` hooks fire as Git's would.
//!
//! The test skips gracefully when `git`, the `grit` binary, or the
//! `grit-http-server` binary is unavailable, or the server fails to bind — the
//! happy path is otherwise real end-to-end authenticated HTTP wire I/O.
//!
//! Gated on the `http-ureq` feature (the default `UreqHttpClient` lives there):
//!   cargo test -p grit-lib --features http-ureq --test transport_http_auth

#![cfg(feature = "http-ureq")]

use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use grit_lib::credentials::{Credential, CredentialProvider};
use grit_lib::error::{Error, Result as GritResult};
use grit_lib::fetch::NoProgress;
use grit_lib::objects::ObjectId;
use grit_lib::odb::Odb;
use grit_lib::push::push_http;
use grit_lib::push_report::PushRefStatus;
use grit_lib::refs::resolve_ref;
use grit_lib::transfer::{FetchOptions, PushOptions, PushRefSpec, TagMode};
use grit_lib::transport::http::http_fetch;
use grit_lib::transport::http::ureq_client::UreqHttpClient;

const USER: &str = "alice";
const PASS: &str = "s3cr3t";

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

fn find_binary(name: &str) -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let deps = exe.parent()?; // deps
    let profile = deps.parent()?; // <profile>
    for cand in [profile.join(name), deps.join(name)] {
        if cand.is_file() {
            return Some(cand);
        }
    }
    None
}

/// Spawn `grit-http-server --root <root> --bind … --require-auth user:pass`.
fn spawn_authed_server(
    server_bin: &Path,
    grit_bin: &Path,
    root: &Path,
    port: u16,
) -> Option<Child> {
    Command::new(server_bin)
        .arg("--root")
        .arg(root)
        .arg("--bind")
        .arg(format!("127.0.0.1:{port}"))
        .arg("--require-auth")
        .arg(format!("{USER}:{PASS}"))
        .env("GUST_BIN", grit_bin)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .ok()
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

struct ServerGuard(Child);
impl Drop for ServerGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// A non-interactive [`CredentialProvider`] that returns a fixed username/password
/// and counts `fill`/`approve`/`reject` calls so the test can prove the auth
/// lifecycle ran (Git's `credential_fill` → `credential_approve`/`reject`).
struct StaticCredentialProvider {
    username: String,
    password: String,
    fills: AtomicUsize,
    approves: AtomicUsize,
    rejects: AtomicUsize,
}

impl StaticCredentialProvider {
    fn new(username: &str, password: &str) -> Self {
        Self {
            username: username.to_owned(),
            password: password.to_owned(),
            fills: AtomicUsize::new(0),
            approves: AtomicUsize::new(0),
            rejects: AtomicUsize::new(0),
        }
    }
}

impl CredentialProvider for StaticCredentialProvider {
    fn fill(&self, input: &Credential) -> GritResult<Credential> {
        self.fills.fetch_add(1, Ordering::SeqCst);
        let mut out = input.clone();
        out.username = Some(self.username.clone());
        out.password = Some(self.password.clone());
        Ok(out)
    }

    fn approve(&self, _cred: &Credential) -> GritResult<()> {
        self.approves.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    fn reject(&self, _cred: &Credential) -> GritResult<()> {
        self.rejects.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

/// Wrap a provider in an `Arc` so the test can observe its counters while it is
/// also owned (boxed) by the client.
#[derive(Clone)]
struct SharedProvider(Arc<StaticCredentialProvider>);

impl CredentialProvider for SharedProvider {
    fn fill(&self, input: &Credential) -> GritResult<Credential> {
        self.0.fill(input)
    }
    fn approve(&self, cred: &Credential) -> GritResult<()> {
        self.0.approve(cred)
    }
    fn reject(&self, cred: &Credential) -> GritResult<()> {
        self.0.reject(cred)
    }
}

/// Mirror a work tree into a bare repo under `root`, served at `/<name>`.
fn mirror_bare(work: &Path, root: &Path, name: &str) -> PathBuf {
    std::fs::create_dir_all(root).unwrap();
    let bare = root.join(name);
    git(
        work,
        &[
            "clone",
            "-q",
            "--bare",
            ".",
            bare.to_str().expect("utf8 path"),
        ],
    );
    git(&bare, &["symbolic-ref", "HEAD", "refs/heads/main"]);
    bare
}

fn make_bare_target(root: &Path, name: &str) -> PathBuf {
    let bare = root.join(name);
    std::fs::create_dir_all(&bare).unwrap();
    git(&bare, &["init", "-q", "--bare", "."]);
    git(&bare, &["symbolic-ref", "HEAD", "refs/heads/main"]);
    bare
}

/// Set up `git`/server binaries + an authed server over `root`. Returns the
/// server guard + port, or `None` if anything required is unavailable (the
/// caller then skips). `root` must already contain the served repo(s).
fn start_authed_server(root: &Path) -> Option<(ServerGuard, u16)> {
    let grit_bin = find_binary("grit")?;
    let server_bin = find_binary("grit-http-server")?;
    let port = free_port()?;
    let child = spawn_authed_server(&server_bin, &grit_bin, root, port)?;
    let guard = ServerGuard(child);
    if !wait_ready(port) {
        return None;
    }
    Some((guard, port))
}

#[test]
fn fetch_over_authed_http_succeeds_with_right_credentials_and_fails_typed_otherwise() {
    if find_binary("grit").is_none() || find_binary("grit-http-server").is_none() {
        eprintln!("SKIP: `grit` / `grit-http-server` binary not found (build them first)");
        return;
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let work = tmp.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    build_source(&work);

    let root = tmp.path().join("srv");
    let source = mirror_bare(&work, &root, "repo.git");
    let main_oid = rev_parse(&source, "refs/heads/main");
    let topic_oid = rev_parse(&source, "refs/heads/topic");
    let c1_oid = rev_parse(&work, "HEAD~1");
    let tag_oid = rev_parse(&source, "refs/tags/v1");

    let Some((_guard, port)) = start_authed_server(&root) else {
        eprintln!("SKIP: could not start authed grit-http-server");
        return;
    };
    let url = format!("http://127.0.0.1:{port}/repo.git");

    let opts = FetchOptions {
        refspecs: vec!["+refs/heads/*:refs/remotes/origin/*".to_owned()],
        tags: TagMode::All,
        ..Default::default()
    };

    // --- 0. Sanity: the server really demands auth. An unauthenticated client
    // must fail with the typed `Error::Auth` (NOT a hang, NOT a generic error). --
    {
        let local = tmp.path().join("noauth");
        std::fs::create_dir_all(&local).unwrap();
        git(&local, &["init", "-q", "-b", "main", "."]);
        let local_git = local.join(".git");
        let client = UreqHttpClient::new().with_git_protocol("version=2");
        let err = http_fetch(&client, &local_git, &url, &opts, &mut NoProgress)
            .expect_err("fetch with no credentials against an authed server must fail");
        assert!(
            matches!(err, Error::Auth(_)),
            "expected a typed Error::Auth without credentials, got: {err:?}"
        );
    }

    // --- 1. WRONG credentials → typed auth error, reject() fired ----------------
    {
        let local = tmp.path().join("wrong");
        std::fs::create_dir_all(&local).unwrap();
        git(&local, &["init", "-q", "-b", "main", "."]);
        let local_git = local.join(".git");

        let provider = SharedProvider(Arc::new(StaticCredentialProvider::new(USER, "wrong-pass")));
        let client = UreqHttpClient::with_credentials(Box::new(provider.clone()))
            .with_git_protocol("version=2");
        let err = http_fetch(&client, &local_git, &url, &opts, &mut NoProgress)
            .expect_err("fetch with wrong credentials must fail");
        assert!(
            matches!(err, Error::Auth(_)),
            "expected a typed Error::Auth for wrong credentials, got: {err:?}"
        );
        // We filled once and, on the 401 retry, rejected the bad credential.
        assert_eq!(
            provider.0.fills.load(Ordering::SeqCst),
            1,
            "fill should run once"
        );
        assert_eq!(
            provider.0.rejects.load(Ordering::SeqCst),
            1,
            "wrong credentials should be rejected"
        );
        assert_eq!(
            provider.0.approves.load(Ordering::SeqCst),
            0,
            "wrong credentials must never be approved"
        );
        // Nothing was fetched.
        assert!(resolve_ref(&local_git, "refs/remotes/origin/main").is_err());
    }

    // --- 2. RIGHT credentials → fetch succeeds, approve() fired -----------------
    let local = tmp.path().join("ok");
    std::fs::create_dir_all(&local).unwrap();
    git(&local, &["init", "-q", "-b", "main", "."]);
    let local_git = local.join(".git");

    let provider = SharedProvider(Arc::new(StaticCredentialProvider::new(USER, PASS)));
    let client =
        UreqHttpClient::with_credentials(Box::new(provider.clone())).with_git_protocol("version=2");
    let outcome = http_fetch(&client, &local_git, &url, &opts, &mut NoProgress)
        .expect("authed fetch with correct credentials must succeed");

    // Credentials were filled and approved (Git's credential_approve on success);
    // a connection-level auth cache means at most one fill regardless of POSTs.
    assert!(
        provider.0.fills.load(Ordering::SeqCst) >= 1,
        "fill should run at least once"
    );
    assert!(
        provider.0.approves.load(Ordering::SeqCst) >= 1,
        "correct credentials should be approved"
    );
    assert_eq!(
        provider.0.rejects.load(Ordering::SeqCst),
        0,
        "correct credentials must not be rejected"
    );

    // Refs + tag landed.
    assert_eq!(
        resolve_ref(&local_git, "refs/remotes/origin/main").expect("origin/main"),
        main_oid
    );
    assert_eq!(
        resolve_ref(&local_git, "refs/remotes/origin/topic").expect("origin/topic"),
        topic_oid
    );
    assert_eq!(
        resolve_ref(&local_git, "refs/tags/v1").expect("tag v1"),
        tag_oid
    );
    assert!(outcome
        .updates
        .iter()
        .any(|u| u.remote_ref == "refs/heads/main" && u.new_oid == Some(main_oid)));

    // Objects landed.
    let local_odb = open_odb(&local_git);
    for oid in [main_oid, topic_oid, c1_oid, tag_oid] {
        assert!(
            local_odb.exists(&oid),
            "object {} missing after authed fetch",
            oid.to_hex()
        );
    }

    // Cross-check the fetched main tip against system git's view of the source.
    assert_eq!(
        resolve_ref(&local_git, "refs/remotes/origin/main")
            .unwrap()
            .to_hex(),
        git(&source, &["rev-parse", "refs/heads/main"]).trim()
    );

    // The fetched pack fsck's clean.
    let fsck = Command::new("git")
        .current_dir(&local)
        .args(["fsck", "--no-dangling"])
        .output()
        .expect("run git fsck");
    assert!(
        fsck.status.success(),
        "git fsck failed after authed fetch: {}",
        String::from_utf8_lossy(&fsck.stderr)
    );
}

#[test]
fn push_over_authed_http_succeeds_with_right_credentials_and_fails_typed_otherwise() {
    if find_binary("grit").is_none() || find_binary("grit-http-server").is_none() {
        eprintln!("SKIP: `grit` / `grit-http-server` binary not found (build them first)");
        return;
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let local = tmp.path().join("local");
    std::fs::create_dir_all(&local).unwrap();
    build_source(&local);
    let local_git = local.join(".git");
    let main_oid = rev_parse(&local, "refs/heads/main");
    let c1_oid = rev_parse(&local, "HEAD~1");

    let root = tmp.path().join("srv");
    let bare = make_bare_target(&root, "push.git");

    let Some((_guard, port)) = start_authed_server(&root) else {
        eprintln!("SKIP: could not start authed grit-http-server");
        return;
    };
    let url = format!("http://127.0.0.1:{port}/push.git");

    let spec = PushRefSpec {
        src: Some(main_oid),
        dst: "refs/heads/main".to_owned(),
        force: false,
        delete: false,
        expected_old: None,
        expect_absent: false,
    };

    // --- 1. NO credentials → typed auth error, remote ref untouched -------------
    {
        let client = UreqHttpClient::new();
        let err = push_http(
            &client,
            &local_git,
            &url,
            std::slice::from_ref(&spec),
            &PushOptions::default(),
            &mut NoProgress,
        )
        .expect_err("push with no credentials against an authed server must fail");
        assert!(
            matches!(err, Error::Auth(_)),
            "expected a typed Error::Auth for an unauthenticated push, got: {err:?}"
        );
        assert!(
            resolve_ref(&bare, "refs/heads/main").is_err(),
            "rejected push must not create the remote ref"
        );
    }

    // --- 2. RIGHT credentials → push succeeds, objects land, fsck clean ---------
    let provider = SharedProvider(Arc::new(StaticCredentialProvider::new(USER, PASS)));
    let client = UreqHttpClient::with_credentials(Box::new(provider.clone()));
    let outcome = push_http(
        &client,
        &local_git,
        &url,
        std::slice::from_ref(&spec),
        &PushOptions::default(),
        &mut NoProgress,
    )
    .expect("authed push with correct credentials must succeed");

    assert_eq!(outcome.results.len(), 1);
    assert_eq!(
        outcome.results[0].status,
        PushRefStatus::Ok,
        "authed push should be accepted, got {:?} ({:?})",
        outcome.results[0].status,
        outcome.results[0].message
    );
    assert!(provider.0.approves.load(Ordering::SeqCst) >= 1);
    assert_eq!(provider.0.rejects.load(Ordering::SeqCst), 0);

    let remote_main = resolve_ref(&bare, "refs/heads/main").expect("remote main written");
    assert_eq!(remote_main, main_oid);

    let remote_odb = open_odb(&bare);
    for oid in [main_oid, c1_oid] {
        assert!(
            remote_odb.exists(&oid),
            "object {} missing from remote odb after authed push",
            oid.to_hex()
        );
    }

    let fsck = Command::new("git")
        .current_dir(&bare)
        .args(["fsck", "--no-dangling"])
        .output()
        .expect("run git fsck");
    assert!(
        fsck.status.success(),
        "git fsck failed after authed push: {}",
        String::from_utf8_lossy(&fsck.stderr)
    );
    // System git agrees on the pushed tip.
    assert_eq!(
        remote_main.to_hex(),
        git(&bare, &["rev-parse", "refs/heads/main"]).trim()
    );
}
