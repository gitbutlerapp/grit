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

use grit_test_support::{free_port, git, rev_parse};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use grit_lib::config::ConfigSet;
use grit_lib::error::{AuthFailure, Error, RemoteUnreadableReason, Result as GritResult};
use grit_lib::objects::odb::Odb;
use grit_lib::refs::resolve_ref;
use grit_lib::remote::credentials::{Credential, CredentialProvider, HelperCredentialProvider};
use grit_lib::remote::fetch::NoProgress;
use grit_lib::remote::push::push_http;
use grit_lib::remote::push_report::PushRefStatus;
use grit_lib::remote::transfer::{FetchOptions, PushOptions, PushRefSpec, TagMode};
use grit_lib::remote::transport::http::ureq_client::UreqHttpClient;
use grit_lib::remote::transport::http::{http_client_arc, http_fetch};

const USER: &str = "alice";
const PASS: &str = "s3cr3t";

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
fn spawn_authed_server(server_bin: &Path, root: &Path, port: u16) -> Option<Child> {
    Command::new(server_bin)
        .arg("--root")
        .arg(root)
        .arg("--bind")
        .arg(format!("127.0.0.1:{port}"))
        .arg("--require-auth")
        .arg(format!("{USER}:{PASS}"))
        .env_remove("GRIT_BIN")
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
    let server_bin = find_binary("grit-http-server")?;
    let port = free_port()?;
    let child = spawn_authed_server(&server_bin, root, port)?;
    let guard = ServerGuard(child);
    if !wait_ready(port) {
        return None;
    }
    Some((guard, port))
}

#[test]
fn fetch_over_authed_http_succeeds_with_right_credentials_and_fails_typed_otherwise() {
    if find_binary("grit-http-server").is_none() {
        eprintln!("SKIP: `grit-http-server` binary not found (build it first)");
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
    let url_with_embedded_creds =
        format!("http://{USER}:wrong-embedded-pass@127.0.0.1:{port}/repo.git");

    let mut opts = {
        let mut o = FetchOptions::default();
        o.refspecs = vec!["+refs/heads/*:refs/remotes/origin/*".to_owned()];
        o.tags = TagMode::All;
        o
    };

    // --- 0. Sanity: the server really demands auth. An unauthenticated client
    // must fail with the typed `Error::Auth` (NOT a hang, NOT a generic error). --
    {
        let local = tmp.path().join("noauth");
        std::fs::create_dir_all(&local).unwrap();
        git(&local, &["init", "-q", "-b", "main", "."]);
        let local_git = local.join(".git");
        let client = http_client_arc(UreqHttpClient::new().with_git_protocol("version=2"));
        let err = http_fetch(client.clone(), &local_git, &url, &mut opts, &mut NoProgress)
            .expect_err("fetch with no credentials against an authed server must fail");
        assert!(
            matches!(&err, Error::Auth { reason, .. } if !reason.is_rejected()),
            "expected a typed Error::Auth without credentials, got: {err:?}"
        );
    }

    // --- 0b. Embedded URL credentials must not appear in auth error text ---------
    {
        let local = tmp.path().join("embedded-creds-url");
        std::fs::create_dir_all(&local).unwrap();
        git(&local, &["init", "-q", "-b", "main", "."]);
        let local_git = local.join(".git");
        let client = http_client_arc(UreqHttpClient::new().with_git_protocol("version=2"));
        let err = http_fetch(
            client,
            &local_git,
            &url_with_embedded_creds,
            &mut opts,
            &mut NoProgress,
        )
        .expect_err("fetch with embedded wrong creds in URL must fail");
        assert!(matches!(err, Error::Auth { .. }));
        let msg = err.to_string();
        assert!(
            !msg.contains("wrong-embedded-pass"),
            "auth error must redact password embedded in remote URL: {msg}"
        );
        assert!(
            !msg.contains(&format!("{USER}:wrong-embedded-pass")),
            "auth error must redact userinfo embedded in remote URL: {msg}"
        );
    }

    // --- 1. WRONG credentials → typed auth error, reject() fired ----------------
    {
        let local = tmp.path().join("wrong");
        std::fs::create_dir_all(&local).unwrap();
        git(&local, &["init", "-q", "-b", "main", "."]);
        let local_git = local.join(".git");

        let provider = SharedProvider(Arc::new(StaticCredentialProvider::new(USER, "wrong-pass")));
        let client = http_client_arc(
            UreqHttpClient::with_credentials(Box::new(provider.clone()))
                .with_git_protocol("version=2"),
        );
        let err = http_fetch(client.clone(), &local_git, &url, &mut opts, &mut NoProgress)
            .expect_err("fetch with wrong credentials must fail");
        assert!(
            matches!(
                err,
                Error::Auth {
                    reason: AuthFailure::Rejected,
                    ..
                }
            ),
            "expected a typed Error::Auth for wrong credentials, got: {err:?}"
        );
        let msg = err.to_string();
        assert!(
            !msg.contains("wrong-pass"),
            "auth error must not echo credentials: {msg}"
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
    let client = http_client_arc(
        UreqHttpClient::with_credentials(Box::new(provider.clone())).with_git_protocol("version=2"),
    );
    let outcome = http_fetch(client.clone(), &local_git, &url, &mut opts, &mut NoProgress)
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

/// Authenticated discovery against a missing repository must not erase credentials:
/// Git runs `credential reject` only on 401, not on 404/403/5xx.
#[test]
fn fetch_missing_repo_with_valid_credentials_does_not_reject_them() {
    if find_binary("grit-http-server").is_none() {
        eprintln!("SKIP: `grit-http-server` binary not found (build it first)");
        return;
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let work = tmp.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    build_source(&work);

    let root = tmp.path().join("srv");
    mirror_bare(&work, &root, "repo.git");

    let Some((_guard, port)) = start_authed_server(&root) else {
        eprintln!("SKIP: could not start authed grit-http-server");
        return;
    };
    let good_url = format!("http://127.0.0.1:{port}/repo.git");
    let typo_url = format!("http://127.0.0.1:{port}/repo-typo.git");

    let mut opts = {
        let mut o = FetchOptions::default();
        o.refspecs = vec!["+refs/heads/*:refs/remotes/origin/*".to_owned()];
        o.tags = TagMode::All;
        o
    };

    let provider = SharedProvider(Arc::new(StaticCredentialProvider::new(USER, PASS)));
    let client = http_client_arc(
        UreqHttpClient::with_credentials(Box::new(provider.clone())).with_git_protocol("version=2"),
    );

    let local = tmp.path().join("typo");
    std::fs::create_dir_all(&local).unwrap();
    git(&local, &["init", "-q", "-b", "main", "."]);
    let local_git = local.join(".git");

    let err = http_fetch(
        client.clone(),
        &local_git,
        &typo_url,
        &mut opts,
        &mut NoProgress,
    )
    .expect_err("fetch of a missing repo must fail");
    assert!(
        matches!(
            err,
            Error::RemoteUnreadable {
                reason: RemoteUnreadableReason::HttpStatus { status: 404 },
                ..
            }
        ),
        "expected HTTP 404 remote_unreadable, got: {err:?}"
    );
    assert_eq!(
        provider.0.rejects.load(Ordering::SeqCst),
        0,
        "404 must not reject stored credentials"
    );
    assert_eq!(
        provider.0.approves.load(Ordering::SeqCst),
        0,
        "404 must not approve credentials either"
    );

    let local_ok = tmp.path().join("ok");
    std::fs::create_dir_all(&local_ok).unwrap();
    git(&local_ok, &["init", "-q", "-b", "main", "."]);
    let local_ok_git = local_ok.join(".git");
    http_fetch(client, &local_ok_git, &good_url, &mut opts, &mut NoProgress)
        .expect("fetch of the real repo must still work with the same provider");
    assert!(
        provider.0.approves.load(Ordering::SeqCst) >= 1,
        "correct credentials should be approved after a successful fetch"
    );
    assert_eq!(
        provider.0.rejects.load(Ordering::SeqCst),
        0,
        "credentials must still not be rejected"
    );
}

#[test]
fn push_over_authed_http_succeeds_with_right_credentials_and_fails_typed_otherwise() {
    if find_binary("grit-http-server").is_none() {
        eprintln!("SKIP: `grit-http-server` binary not found (build it first)");
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
        src_ref: None,
        dst: "refs/heads/main".to_owned(),
        force: false,
        delete: false,
        expected_old: None,
        expect_absent: false,
        ..Default::default()
    };

    // --- 1. NO credentials → typed auth error, remote ref untouched -------------
    {
        let client = http_client_arc(UreqHttpClient::new());
        let err = push_http(
            client.clone(),
            &local_git,
            &url,
            std::slice::from_ref(&spec),
            &PushOptions::default(),
            &mut NoProgress,
            &grit_lib::progress::NeverCancel,
        )
        .expect_err("push with no credentials against an authed server must fail");
        assert!(
            matches!(&err, Error::Auth { reason, .. } if !reason.is_rejected()),
            "expected a typed Error::Auth for an unauthenticated push, got: {err:?}"
        );
        assert!(
            resolve_ref(&bare, "refs/heads/main").is_err(),
            "rejected push must not create the remote ref"
        );
    }

    // --- 2. RIGHT credentials → push succeeds, objects land, fsck clean ---------
    let provider = SharedProvider(Arc::new(StaticCredentialProvider::new(USER, PASS)));
    let client = http_client_arc(UreqHttpClient::with_credentials(Box::new(provider.clone())));
    let outcome = push_http(
        client.clone(),
        &local_git,
        &url,
        std::slice::from_ref(&spec),
        &PushOptions::default(),
        &mut NoProgress,
        &grit_lib::progress::NeverCancel,
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

/// The system `git-credential-store`, or `None` when Git does not ship it.
fn system_credential_store() -> Option<PathBuf> {
    let out = Command::new("git").arg("--exec-path").output().ok()?;
    let store =
        PathBuf::from(String::from_utf8_lossy(&out.stdout).trim()).join("git-credential-store");
    store.is_file().then_some(store)
}

/// A host-level `git-credential-store` entry (no path, as `git credential
/// approve` writes it) authenticates fetch and push to any repository on the
/// host, and helpers receive only `protocol` and `host`.
#[test]
fn credential_store_host_entry_authenticates_fetch_and_push() {
    if find_binary("grit-http-server").is_none() {
        eprintln!("SKIP: `grit-http-server` binary not found (build it first)");
        return;
    }
    if system_credential_store().is_none() {
        eprintln!("SKIP: system git-credential-store not available");
        return;
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let work = tmp.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    build_source(&work);
    let root = tmp.path().join("srv");
    let source = mirror_bare(&work, &root, "hw.git");
    let push_target = make_bare_target(&root, "push.git");
    let main_oid = rev_parse(&source, "refs/heads/main");

    let Some((_guard, port)) = start_authed_server(&root) else {
        eprintln!("SKIP: could not start authed grit-http-server");
        return;
    };
    let host = format!("127.0.0.1:{port}");

    // System git writes the store entry; grit has to read it.
    let store_file = tmp.path().join("git-credentials");
    let store_helper = format!("store --file {}", store_file.display());
    let approve = Command::new("git")
        .args(["-c", &format!("credential.helper={store_helper}")])
        .args(["credential", "approve"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .stdin(Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write as _;
            child.stdin.take().expect("stdin").write_all(
                format!("protocol=http\nhost={host}\nusername={USER}\npassword={PASS}\n\n")
                    .as_bytes(),
            )?;
            child.wait()
        })
        .expect("run git credential approve");
    assert!(approve.success(), "git credential approve failed");
    let stored = std::fs::read_to_string(&store_file).expect("store file");
    assert_eq!(stored.lines().count(), 1, "one host-level entry: {stored}");

    let helper_log = tmp.path().join("helper.log");
    let mut config = ConfigSet::new();
    config
        .add_command_override(
            "credential.helper",
            &format!("!cat >>'{}'; :", helper_log.display()),
        )
        .unwrap();
    config
        .add_command_override("credential.helper", &store_helper)
        .unwrap();
    let client = || {
        http_client_arc(
            UreqHttpClient::from_config(&config)
                .expect("client from config")
                .with_credential_provider(Box::new(HelperCredentialProvider::new(config.clone()))),
        )
    };

    let local = tmp.path().join("clone");
    std::fs::create_dir_all(&local).unwrap();
    git(&local, &["init", "-q", "-b", "main", "."]);
    let local_git = local.join(".git");
    let mut opts = {
        let mut o = FetchOptions::default();
        o.refspecs = vec!["+refs/heads/*:refs/remotes/origin/*".to_owned()];
        o.tags = TagMode::All;
        o
    };
    http_fetch(
        client(),
        &local_git,
        &format!("http://{host}/hw.git"),
        &mut opts,
        &mut NoProgress,
    )
    .expect("fetch authenticated by the host-level store entry");
    assert_eq!(
        resolve_ref(&local_git, "refs/remotes/origin/main").expect("origin/main"),
        main_oid
    );

    let spec = PushRefSpec {
        src: Some(main_oid),
        src_ref: None,
        dst: "refs/heads/main".to_owned(),
        force: false,
        delete: false,
        expected_old: None,
        expect_absent: false,
        ..Default::default()
    };
    let outcome = push_http(
        client(),
        &local_git,
        &format!("http://{host}/push.git"),
        std::slice::from_ref(&spec),
        &PushOptions::default(),
        &mut NoProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("push authenticated by the host-level store entry");
    assert_eq!(outcome.results[0].status, PushRefStatus::Ok);
    assert_eq!(
        resolve_ref(&push_target, "refs/heads/main").expect("pushed main"),
        main_oid
    );

    let log = std::fs::read_to_string(&helper_log).expect("helper log");
    let get_request = format!("protocol=http\nhost={host}\n\n");
    assert!(
        log.starts_with(&get_request),
        "helpers must get only protocol and host, got:\n{log}"
    );
    assert!(!log.contains("url="), "helpers must never get url=:\n{log}");
    assert!(
        !log.contains("path="),
        "path needs credential.useHttpPath:\n{log}"
    );

    // Approving the working credential leaves the host-level entry as the only one.
    assert_eq!(
        std::fs::read_to_string(&store_file).expect("store file"),
        stored
    );
}

/// An askpass script that answers `USER` for the username prompt and `PASS`
/// otherwise, appending every prompt it receives to `log`.
#[cfg(unix)]
fn write_askpass(dir: &Path, log: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt as _;
    let script = dir.join("askpass.sh");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$1\" >>'{}'\ncase \"$1\" in Username*) echo {USER};; *) echo {PASS};; esac\n",
            log.display()
        ),
    )
    .unwrap();
    let mut perms = std::fs::metadata(&script).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&script, perms).unwrap();
    script
}

fn shared_client(
    factory: &grit_lib::remote::DefaultHttpClientFactory,
    config: &ConfigSet,
) -> Arc<dyn grit_lib::remote::transport::http::HttpClient> {
    use grit_lib::remote::HttpClientFactory as _;
    Arc::from(factory.create(config).expect("client from factory"))
}

fn git_fsck(git_dir: &Path) {
    let fsck = Command::new("git")
        .arg("--git-dir")
        .arg(git_dir)
        .args(["fsck", "--no-dangling"])
        .output()
        .expect("run git fsck");
    assert!(
        fsck.status.success(),
        "git fsck failed: {}",
        String::from_utf8_lossy(&fsck.stderr)
    );
}

/// `GIT_ASKPASS` answers the auth challenge when no helper does: grit asks the
/// same prompts as system `git` with the same script, the fetch lands objects
/// `git fsck` accepts, and the askpass credential is stored with the helpers.
#[cfg(unix)]
#[test]
fn git_askpass_authenticates_fetch_with_the_prompts_system_git_uses() {
    if find_binary("grit-http-server").is_none() {
        eprintln!("SKIP: `grit-http-server` binary not found (build it first)");
        return;
    }
    if system_credential_store().is_none() {
        eprintln!("SKIP: system git-credential-store not available");
        return;
    }
    let tmp = tempfile::tempdir().expect("tempdir");
    let work = tmp.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    build_source(&work);
    let root = tmp.path().join("srv");
    let source = mirror_bare(&work, &root, "hello.git");
    let main_oid = rev_parse(&source, "refs/heads/main");
    let Some((_guard, port)) = start_authed_server(&root) else {
        eprintln!("SKIP: could not start authed grit-http-server");
        return;
    };
    let url = format!("http://127.0.0.1:{port}/hello.git");

    let git_log = tmp.path().join("git-askpass.log");
    let git_askpass_dir = tmp.path().join("git-askpass");
    std::fs::create_dir_all(&git_askpass_dir).unwrap();
    let git_askpass = write_askpass(&git_askpass_dir, &git_log);
    let git_clone = Command::new("git")
        .args(["clone", "-q", &url])
        .arg(tmp.path().join("git-clone"))
        .env("GIT_ASKPASS", &git_askpass)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env_remove("SSH_ASKPASS")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("run git clone");
    assert!(
        git_clone.status.success(),
        "system git clone with GIT_ASKPASS failed: {}",
        String::from_utf8_lossy(&git_clone.stderr)
    );

    let grit_log = tmp.path().join("grit-askpass.log");
    let askpass = write_askpass(tmp.path(), &grit_log);
    let store_file = tmp.path().join("git-credentials");
    let mut config = ConfigSet::new();
    config
        .add_command_override(
            "credential.helper",
            &format!("store --file {}", store_file.display()),
        )
        .unwrap();
    let env = grit_lib::config::environment::Environment::from_vars(
        [(
            std::ffi::OsString::from("GIT_ASKPASS"),
            askpass.clone().into_os_string(),
        )],
        tmp.path().to_path_buf(),
    );
    let factory = grit_lib::remote::DefaultHttpClientFactory::with_environment(env);

    let local = tmp.path().join("grit-clone");
    std::fs::create_dir_all(&local).unwrap();
    git(&local, &["init", "-q", "-b", "main", "."]);
    let local_git = local.join(".git");
    let mut opts = {
        let mut o = FetchOptions::default();
        o.refspecs = vec!["+refs/heads/*:refs/remotes/origin/*".to_owned()];
        o.tags = TagMode::All;
        o
    };
    http_fetch(
        shared_client(&factory, &config),
        &local_git,
        &url,
        &mut opts,
        &mut NoProgress,
    )
    .expect("fetch authenticated through GIT_ASKPASS");
    assert_eq!(
        resolve_ref(&local_git, "refs/remotes/origin/main").expect("origin/main"),
        main_oid
    );
    git_fsck(&local_git);

    assert_eq!(
        std::fs::read_to_string(&grit_log).expect("grit askpass log"),
        std::fs::read_to_string(&git_log).expect("git askpass log"),
        "grit must ask askpass the same prompts as system git"
    );
    let filled = Command::new("git")
        .args([
            "-c",
            &format!("credential.helper=store --file {}", store_file.display()),
        ])
        .args(["credential", "fill"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env_remove("GIT_ASKPASS")
        .env_remove("SSH_ASKPASS")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write as _;
            child
                .stdin
                .take()
                .expect("stdin")
                .write_all(format!("protocol=http\nhost=127.0.0.1:{port}\n\n").as_bytes())?;
            child.wait_with_output()
        })
        .expect("run git credential fill");
    let filled = String::from_utf8_lossy(&filled.stdout);
    assert!(
        filled.contains(&format!("username={USER}\n"))
            && filled.contains(&format!("password={PASS}\n")),
        "system git must find the askpass credential grit stored: {filled}"
    );
}

/// `core.askPass` works through the default factory without any environment,
/// for push as well as fetch, and the pushed objects pass `git fsck`.
#[cfg(unix)]
#[test]
fn core_askpass_authenticates_push_through_the_default_factory() {
    if find_binary("grit-http-server").is_none() {
        eprintln!("SKIP: `grit-http-server` binary not found (build it first)");
        return;
    }
    let tmp = tempfile::tempdir().expect("tempdir");
    let work = tmp.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    build_source(&work);
    let root = tmp.path().join("srv");
    let push_target = make_bare_target(&root, "push.git");
    let main_oid = rev_parse(&work, "refs/heads/main");
    let Some((_guard, port)) = start_authed_server(&root) else {
        eprintln!("SKIP: could not start authed grit-http-server");
        return;
    };

    let log = tmp.path().join("askpass.log");
    let askpass = write_askpass(tmp.path(), &log);
    let mut config = ConfigSet::new();
    config
        .add_command_override("core.askPass", askpass.to_str().expect("utf8 path"))
        .unwrap();
    let factory = grit_lib::remote::DefaultHttpClientFactory::new();
    let spec = PushRefSpec {
        src: Some(main_oid),
        dst: "refs/heads/main".to_owned(),
        ..Default::default()
    };
    let outcome = push_http(
        shared_client(&factory, &config),
        &work.join(".git"),
        &format!("http://127.0.0.1:{port}/push.git"),
        std::slice::from_ref(&spec),
        &PushOptions::default(),
        &mut NoProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("push authenticated through core.askPass");
    assert_eq!(outcome.results[0].status, PushRefStatus::Ok);
    assert_eq!(
        git(&push_target, &["rev-parse", "refs/heads/main"]).trim(),
        main_oid.to_hex()
    );
    git_fsck(&push_target);
    assert_eq!(
        std::fs::read_to_string(&log).expect("askpass log"),
        format!(
            "Username for 'http://127.0.0.1:{port}': \nPassword for 'http://{USER}@127.0.0.1:{port}': \n"
        )
    );
}

/// Credentials in the URL are sent as they are: wrong ones are reported as
/// rejected (not as unavailable), and askpass is not consulted for them.
#[cfg(unix)]
#[test]
fn wrong_credentials_in_the_url_are_rejected_without_asking_askpass() {
    if find_binary("grit-http-server").is_none() {
        eprintln!("SKIP: `grit-http-server` binary not found (build it first)");
        return;
    }
    let tmp = tempfile::tempdir().expect("tempdir");
    let work = tmp.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    build_source(&work);
    let root = tmp.path().join("srv");
    mirror_bare(&work, &root, "hello.git");
    let Some((_guard, port)) = start_authed_server(&root) else {
        eprintln!("SKIP: could not start authed grit-http-server");
        return;
    };

    let log = tmp.path().join("askpass.log");
    let askpass = write_askpass(tmp.path(), &log);
    let mut config = ConfigSet::new();
    config
        .add_command_override("core.askPass", askpass.to_str().expect("utf8 path"))
        .unwrap();
    let factory = grit_lib::remote::DefaultHttpClientFactory::new();
    let local = tmp.path().join("clone");
    std::fs::create_dir_all(&local).unwrap();
    git(&local, &["init", "-q", "-b", "main", "."]);
    let mut opts = {
        let mut o = FetchOptions::default();
        o.refspecs = vec!["+refs/heads/*:refs/remotes/origin/*".to_owned()];
        o
    };
    let err = http_fetch(
        shared_client(&factory, &config),
        &local.join(".git"),
        &format!("http://{USER}:wrong@127.0.0.1:{port}/hello.git"),
        &mut opts,
        &mut NoProgress,
    )
    .expect_err("wrong URL credentials must fail");
    assert!(
        matches!(
            &err,
            Error::Auth {
                reason: AuthFailure::Rejected,
                ..
            }
        ),
        "expected a rejected-credentials error, got: {err:?}"
    );
    assert!(!err.to_string().contains("wrong"), "secret leaked: {err}");
    assert!(
        !log.exists(),
        "askpass must not run for a complete URL credential"
    );
}

/// A stored credential the server refuses survives the best-effort advertisement
/// [`load_push_plan_context`] reads, so the push itself sends it, reports it as
/// rejected (not as missing), and erases it as Git does. With the right
/// credential stored, the same advertisement authenticates and fills the context.
#[test]
fn a_rejected_stored_credential_is_reported_by_the_push_not_erased_by_the_plan() {
    use grit_lib::config::environment::{Environment, RepositoryOptions};
    use grit_lib::remote::push_plan::{load_push_plan_context, open_push_destination};
    use grit_lib::remote::RemoteError;
    use grit_lib::repo::Repository;

    if find_binary("grit-http-server").is_none() {
        eprintln!("SKIP: `grit-http-server` binary not found (build it first)");
        return;
    }
    if system_credential_store().is_none() {
        eprintln!("SKIP: system git-credential-store not available");
        return;
    }
    let tmp = tempfile::tempdir().expect("tempdir");
    let work = tmp.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    build_source(&work);
    let root = tmp.path().join("srv");
    let push_target = make_bare_target(&root, "push.git");
    let main_oid = rev_parse(&work, "refs/heads/main");
    let Some((_guard, port)) = start_authed_server(&root) else {
        eprintln!("SKIP: could not start authed grit-http-server");
        return;
    };

    let store_file = tmp.path().join("git-credentials");
    let wrong = format!("http://{USER}:WRONG@127.0.0.1%3a{port}\n");
    std::fs::write(&store_file, &wrong).unwrap();
    git(
        &work,
        &[
            "remote",
            "add",
            "origin",
            &format!("http://127.0.0.1:{port}/push.git"),
        ],
    );
    git(
        &work,
        &[
            "config",
            "credential.helper",
            &format!("store --file {}", store_file.display()),
        ],
    );

    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let mut vars: Vec<(std::ffi::OsString, std::ffi::OsString)> = std::env::vars_os()
        .filter(|(k, _)| !k.to_string_lossy().starts_with("GIT_"))
        .collect();
    vars.push(("HOME".into(), home.clone().into()));
    vars.push(("XDG_CONFIG_HOME".into(), home.join(".config").into()));
    vars.push(("GIT_CONFIG_NOSYSTEM".into(), "1".into()));
    let env = Environment::from_vars(vars, work.clone());
    let repo = Repository::open(
        &work.join(".git"),
        &RepositoryOptions::with_environment(env).with_work_tree(&work),
    )
    .expect("open repo");
    let config = repo.config().expect("config");

    let ctx = load_push_plan_context(&repo, &config, Some("origin"), None).expect("context");
    assert!(ctx.remote_tips.is_none(), "the advertisement was refused");
    assert_eq!(
        std::fs::read_to_string(&store_file).unwrap(),
        wrong,
        "planning must not erase the stored credential"
    );

    let spec = PushRefSpec {
        src: Some(main_oid),
        dst: "refs/heads/main".to_owned(),
        ..Default::default()
    };
    let push = || {
        open_push_destination(&config, "origin")
            .expect("origin")
            .push(
                &repo,
                std::slice::from_ref(&spec),
                PushOptions::default(),
                &mut NoProgress,
                &grit_lib::progress::NeverCancel,
                None,
            )
    };
    let err = push().expect_err("the stored credential is wrong");
    assert!(
        matches!(
            &err,
            RemoteError::Library(Error::Auth {
                reason: AuthFailure::Rejected,
                ..
            })
        ),
        "expected the push to report rejected credentials, got: {err:?}"
    );
    assert_eq!(
        std::fs::read_to_string(&store_file).unwrap(),
        "",
        "the push erases the refused credential, as Git does"
    );

    std::fs::write(
        &store_file,
        format!("http://{USER}:{PASS}@127.0.0.1%3a{port}\n"),
    )
    .unwrap();
    let ctx = load_push_plan_context(&repo, &config, Some("origin"), None).expect("context");
    assert_eq!(
        ctx.remote_tips,
        Some(Vec::new()),
        "the empty target was listed"
    );
    let outcome = push().expect("push with the stored credential");
    assert_eq!(outcome.results[0].status, PushRefStatus::Ok);
    assert_eq!(
        git(&push_target, &["rev-parse", "refs/heads/main"]).trim(),
        main_oid.to_hex()
    );
    git_fsck(&push_target);
}

/// A caller's own [`HttpClientFactory`](grit_lib::remote::HttpClientFactory) is
/// asked for a probe client for the push-plan advertisement and a normal client
/// for the push, so only the push rejects the refused credential.
#[test]
fn custom_factories_build_the_push_plan_probe_with_create_for_probe() {
    use grit_lib::config::environment::{Environment, RepositoryOptions};
    use grit_lib::remote::push_plan::{load_push_plan_context, open_push_destination};
    use grit_lib::remote::transport::http::HttpClient;
    use grit_lib::remote::{HttpClientFactory, RemoteError, RemoteResult};
    use grit_lib::repo::Repository;

    /// Fills like the shared provider but drops `reject`.
    struct NoReject(SharedProvider);
    impl CredentialProvider for NoReject {
        fn fill(&self, input: &Credential) -> GritResult<Credential> {
            self.0.fill(input)
        }
        fn approve(&self, cred: &Credential) -> GritResult<()> {
            self.0.approve(cred)
        }
        fn reject(&self, _cred: &Credential) -> GritResult<()> {
            Ok(())
        }
    }

    struct CountingFactory {
        provider: SharedProvider,
        creates: AtomicUsize,
        probes: AtomicUsize,
    }
    impl HttpClientFactory for CountingFactory {
        fn create(&self, config: &ConfigSet) -> RemoteResult<Box<dyn HttpClient>> {
            self.creates.fetch_add(1, Ordering::SeqCst);
            Ok(Box::new(
                UreqHttpClient::from_config(config)
                    .expect("client")
                    .with_credential_provider(Box::new(self.provider.clone())),
            ))
        }
        fn create_for_probe(&self, config: &ConfigSet) -> RemoteResult<Box<dyn HttpClient>> {
            self.probes.fetch_add(1, Ordering::SeqCst);
            Ok(Box::new(
                UreqHttpClient::from_config(config)
                    .expect("client")
                    .with_credential_provider(Box::new(NoReject(self.provider.clone()))),
            ))
        }
    }

    if find_binary("grit-http-server").is_none() {
        eprintln!("SKIP: `grit-http-server` binary not found (build it first)");
        return;
    }
    let tmp = tempfile::tempdir().expect("tempdir");
    let work = tmp.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    build_source(&work);
    let root = tmp.path().join("srv");
    make_bare_target(&root, "push.git");
    let main_oid = rev_parse(&work, "refs/heads/main");
    let Some((_guard, port)) = start_authed_server(&root) else {
        eprintln!("SKIP: could not start authed grit-http-server");
        return;
    };
    git(
        &work,
        &[
            "remote",
            "add",
            "origin",
            &format!("http://127.0.0.1:{port}/push.git"),
        ],
    );
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let mut vars: Vec<(std::ffi::OsString, std::ffi::OsString)> = std::env::vars_os()
        .filter(|(k, _)| !k.to_string_lossy().starts_with("GIT_"))
        .collect();
    vars.push(("HOME".into(), home.clone().into()));
    vars.push(("GIT_CONFIG_NOSYSTEM".into(), "1".into()));
    let repo = Repository::open(
        &work.join(".git"),
        &RepositoryOptions::with_environment(Environment::from_vars(vars, work.clone()))
            .with_work_tree(&work),
    )
    .expect("open repo");
    let config = repo.config().expect("config");

    let counters = Arc::new(StaticCredentialProvider::new(USER, "WRONG"));
    let factory = CountingFactory {
        provider: SharedProvider(counters.clone()),
        creates: AtomicUsize::new(0),
        probes: AtomicUsize::new(0),
    };
    let ctx =
        load_push_plan_context(&repo, &config, Some("origin"), Some(&factory)).expect("context");
    assert!(ctx.remote_tips.is_none());
    assert_eq!(factory.probes.load(Ordering::SeqCst), 1);
    assert_eq!(factory.creates.load(Ordering::SeqCst), 0);
    assert_eq!(counters.fills.load(Ordering::SeqCst), 1);
    assert_eq!(counters.rejects.load(Ordering::SeqCst), 0);

    let spec = PushRefSpec {
        src: Some(main_oid),
        dst: "refs/heads/main".to_owned(),
        ..Default::default()
    };
    let err = open_push_destination(&config, "origin")
        .expect("origin")
        .push(
            &repo,
            std::slice::from_ref(&spec),
            PushOptions::default(),
            &mut NoProgress,
            &grit_lib::progress::NeverCancel,
            Some(&factory),
        )
        .expect_err("wrong credentials");
    assert!(
        matches!(
            &err,
            RemoteError::Library(Error::Auth {
                reason: AuthFailure::Rejected,
                ..
            })
        ),
        "{err:?}"
    );
    assert_eq!(factory.probes.load(Ordering::SeqCst), 1);
    assert!(factory.creates.load(Ordering::SeqCst) >= 1);
    assert_eq!(counters.rejects.load(Ordering::SeqCst), 1);
}
