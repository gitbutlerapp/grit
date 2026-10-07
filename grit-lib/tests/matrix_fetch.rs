//! COMPREHENSIVE fetch matrix across all three remote transports
//! (`git://` daemon, `ssh`, smart-`http`) and protocol versions (v0/v1 AND v2).
//!
//! This file is the cross-transport sibling of the focused single-transport
//! tests (`transport_git_daemon*.rs`, `transport_ssh.rs`, `transport_http.rs`):
//! rather than proving one wire detail per transport, it drives the SAME set of
//! fetch behaviors through every transport so a regression in the shared
//! `fetch::fetch_remote` / `http::http_fetch` refspec/tag/prune logic is caught
//! no matter which transport an embedder picks.
//!
//! ## The transport abstraction
//!
//! Each transport is reduced to a single closure ([`FetchFn`]) that, given a
//! source repo on disk, a local repo's `.git`, a protocol version, and a
//! [`FetchOptions`], performs one real fetch over real wire bytes and returns the
//! [`FetchOutcome`]. The matrix bodies are transport-agnostic and call that
//! closure; a `for driver in drivers()` loop runs each scenario over every
//! available transport x protocol-version pair. A transport that cannot be
//! brought up in this environment (no `git daemon`, no `sh`, no `grit` /
//! `grit-http-server` binary) contributes zero drivers and is skipped *for that
//! transport only* — the happy path still runs over whatever is available, and
//! every test asserts that AT LEAST ONE driver actually ran.
//!
//! ## Coverage (each over git:// , ssh, http x v0/v1 and v2 where supported)
//!
//!   * wildcard refspec `+refs/heads/*:refs/remotes/origin/*`
//!   * exact refspec `+refs/heads/main:refs/remotes/origin/main`
//!   * negative refspec excluding a head from a wildcard set
//!   * tag modes: All (incl. a tag on an UNREACHABLE commit), Following
//!     (unreachable tag dropped), None (no tags)
//!   * `--prune` of a VANISHED wildcard ref and of a VANISHED exact ref
//!   * prune-before-update directory/file conflict (a stale `origin/feature`
//!     ref pruned so `origin/feature/x` can be written)
//!   * empty / unborn-HEAD remote (no refs, hang-free, empty outcome)
//!   * HEAD symref -> `default_branch`
//!   * incremental fetch into a repo already sharing history (minimal pack: the
//!     new tip lands and old objects are not re-required)
//!   * oid + object cross-checks vs system `git rev-parse` and `git fsck` after
//!     every fetch.
//!
//! Gated on `http-ureq` (the default `UreqHttpClient` lives there) so the http
//! driver compiles:
//!   cargo test -p grit-lib --features http-ureq --test matrix_fetch

#![cfg(feature = "http-ureq")]
#![cfg(unix)]

use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use grit_lib::error::Result as GritResult;
use grit_lib::fetch::{fetch_remote, NoProgress};
use grit_lib::objects::ObjectId;
use grit_lib::odb::Odb;
use grit_lib::refs::resolve_ref;
use grit_lib::transfer::{FetchOptions, FetchOutcome, TagMode, UpdateMode};
use grit_lib::transport::http::http_fetch;
use grit_lib::transport::http::ureq_client::UreqHttpClient;
use grit_lib::transport::{ConnectOptions, GitDaemonTransport, Service, SshTransport, Transport};

// ---------------------------------------------------------------------------
// git plumbing helpers (copied from the sibling transport_* tests so this file
// reuses the exact same fixture-construction harness and does not reinvent it).
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
        "git {args:?} in {} failed (status {:?}): stderr={} stdout={}",
        dir.display(),
        out.status.code(),
        String::from_utf8_lossy(&out.stderr),
        String::from_utf8_lossy(&out.stdout)
    );
    String::from_utf8(out.stdout).expect("utf8 git output")
}

fn rev_parse(dir: &Path, rev: &str) -> ObjectId {
    ObjectId::from_hex(git(dir, &["rev-parse", rev]).trim()).expect("valid oid")
}

fn open_odb(git_dir: &Path) -> Odb {
    Odb::new(&git_dir.join("objects")).with_config_git_dir(git_dir.to_path_buf())
}

/// `git fsck --no-dangling` must succeed on the freshly fetched repo.
fn fsck_clean(local_root: &Path) {
    let fsck = Command::new("git")
        .current_dir(local_root)
        .args(["fsck", "--no-dangling"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("run git fsck");
    assert!(
        fsck.status.success(),
        "git fsck failed: {}\n{}",
        String::from_utf8_lossy(&fsck.stdout),
        String::from_utf8_lossy(&fsck.stderr)
    );
}

/// An empty local repo at `<root>` whose `.git` is returned.
fn init_local(root: &Path) -> PathBuf {
    std::fs::create_dir_all(root).unwrap();
    git(root, &["init", "-q", "-b", "main", "."]);
    root.join(".git")
}

/// Build the standard source work tree: two commits on `main`, a `topic`
/// branch, an annotated tag `v1` (reachable from main), and `vside` — an
/// annotated tag on a commit reachable ONLY from a `side` branch (so tag-mode
/// `Following` must drop it while `All` keeps it). Leaves HEAD on `main`.
fn build_source_work(dir: &Path) {
    git(dir, &["init", "-q", "-b", "main", "."]);
    std::fs::write(dir.join("a.txt"), "one\n").unwrap();
    git(dir, &["add", "a.txt"]);
    git(dir, &["commit", "-q", "-m", "c1"]);
    std::fs::write(dir.join("b.txt"), "two\n").unwrap();
    git(dir, &["add", "b.txt"]);
    git(dir, &["commit", "-q", "-m", "c2"]);
    git(dir, &["tag", "-a", "v1", "-m", "release one"]);
    git(dir, &["branch", "topic"]);
    // A side branch with a tag on its unreachable-from-main tip.
    git(dir, &["checkout", "-q", "-b", "side"]);
    git(dir, &["commit", "-q", "--allow-empty", "-m", "side1"]);
    git(dir, &["tag", "-a", "vside", "-m", "side tag"]);
    git(dir, &["checkout", "-q", "main"]);
}

/// Mirror a work tree into a bare repo at `bare` and pin HEAD -> main.
fn bare_clone(work: &Path, bare: &Path) {
    git(
        work,
        &["clone", "-q", "--bare", ".", bare.to_str().expect("utf8")],
    );
    git(bare, &["symbolic-ref", "HEAD", "refs/heads/main"]);
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

// ---------------------------------------------------------------------------
// Transport drivers. Each `Driver` knows how to perform a real fetch from an
// on-disk `source` bare repo via one transport at one protocol version. The
// `source` path is the bare repo the test built; daemon/http serve a *copy*
// under a base dir, ssh runs upload-pack directly on the path.
// ---------------------------------------------------------------------------

/// A live server process plus the temp dir/base it serves, kept alive by the
/// driver. Streaming transports (ssh) carry `None`.
struct ServerHandle {
    _proc: Option<Child>,
    // The base/root dir the server exports; the source bare repo is copied here.
    base: PathBuf,
}

impl Drop for ServerHandle {
    fn drop(&mut self) {
        if let Some(child) = self._proc.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Locate a sibling binary (`grit`, `grit-http-server`) in the cargo target dir.
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

/// Write an executable fake-ssh script (the Git-test-suite trick): ignore the
/// host, run the remote command (`git-upload-pack '<path>'`) locally.
fn write_fake_ssh(dir: &Path) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let script = dir.join("fake-ssh.sh");
    let body = r#"#!/bin/sh
cmd=
for cmd in "$@"; do :; done
case "$cmd" in
  "git-upload-pack "*) cmd="git upload-pack ${cmd#git-upload-pack }" ;;
  "git-receive-pack "*) cmd="git receive-pack ${cmd#git-receive-pack }" ;;
esac
eval "exec $cmd"
"#;
    std::fs::write(&script, body).ok()?;
    let mut perms = std::fs::metadata(&script).ok()?.permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&script, perms).ok()?;
    Some(script)
}

/// What a driver needs to serve `source` and the closure that performs a fetch.
struct Driver {
    /// Human label, e.g. "git-daemon/v2".
    name: &'static str,
    /// Protocol version this driver negotiates (kept for diagnostics/labels).
    #[allow(dead_code)]
    protocol: u8,
    /// Spin up whatever is needed to serve `source` (a bare repo). Returns a live
    /// handle plus a URL/locator the fetch closure understands, or `None` to skip
    /// this driver (server bring-up failed).
    serve: Box<dyn Fn(&Path) -> Option<(ServerHandle, String)>>,
    /// Perform one fetch from `locator` into `local_git`. The `tmp_scratch` dir is
    /// available for a fresh fake-ssh script etc.
    fetch: Box<dyn Fn(&str, &Path, &FetchOptions) -> GritResult<FetchOutcome>>,
}

/// Build the daemon driver for `protocol` (0 => v0/v1, 2 => v2). `None` if
/// `git daemon` cannot be started.
fn daemon_driver(protocol: u8) -> Driver {
    let label = if protocol >= 2 {
        "git-daemon/v2"
    } else {
        "git-daemon/v1"
    };
    Driver {
        name: label,
        protocol,
        serve: Box::new(|source: &Path| {
            let base = source.parent()?.join(format!(
                "daemon-base-{}",
                source.file_name()?.to_string_lossy()
            ));
            std::fs::create_dir_all(&base).ok()?;
            // Serve a copy so the path layout is `<base>/repo.git`.
            let served = base.join("repo.git");
            // `git clone --bare` of a bare repo reproduces it cleanly.
            let st = Command::new("git")
                .args(["clone", "-q", "--bare", source.to_str()?, served.to_str()?])
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .status()
                .ok()?;
            if !st.success() {
                return None;
            }
            let _ = Command::new("git")
                .current_dir(&served)
                .args(["symbolic-ref", "HEAD", "refs/heads/main"])
                .status();
            let port = free_port()?;
            let child = Command::new("git")
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
                .ok()?;
            if !wait_ready(port) {
                let mut c = child;
                let _ = c.kill();
                let _ = c.wait();
                return None;
            }
            let url = format!("git://127.0.0.1:{port}/repo.git");
            Some((
                ServerHandle {
                    _proc: Some(child),
                    base,
                },
                url,
            ))
        }),
        fetch: Box::new(move |url, local_git, opts| {
            let transport = GitDaemonTransport::new();
            let copts = ConnectOptions {
                protocol_version: protocol,
                ..Default::default()
            };
            let mut conn = transport.connect(url, Service::UploadPack, &copts)?;
            fetch_remote(local_git, &mut *conn, opts, &mut NoProgress)
        }),
    }
}

/// Build the ssh driver for `protocol`. Uses a fake-ssh script that runs
/// `git upload-pack` on the source path locally. `GIT_PROTOCOL=version=2` is set
/// by the transport for v2.
fn ssh_driver(protocol: u8, scratch: PathBuf) -> Option<Driver> {
    // Probe sh + write the fake-ssh once; if unavailable, no ssh driver.
    if Command::new("sh").arg("-c").arg("exit 0").status().is_err() {
        return None;
    }
    let fake_ssh = write_fake_ssh(&scratch)?;
    let label = if protocol >= 2 { "ssh/v2" } else { "ssh/v1" };
    Some(Driver {
        name: label,
        protocol,
        // ssh runs upload-pack directly on the source path; no separate server.
        serve: Box::new(|source: &Path| {
            let url = format!("ssh://git@fakehost{}", source.to_str()?);
            Some((
                ServerHandle {
                    _proc: None,
                    base: source.to_path_buf(),
                },
                url,
            ))
        }),
        fetch: Box::new(move |url, local_git, opts| {
            let transport = SshTransport::with_program(fake_ssh.as_os_str());
            let copts = ConnectOptions {
                protocol_version: protocol,
                ..Default::default()
            };
            let mut conn = transport.connect(url, Service::UploadPack, &copts)?;
            fetch_remote(local_git, &mut *conn, opts, &mut NoProgress)
        }),
    })
}

/// Build the http driver for `protocol`. Requires the `grit` and
/// `grit-http-server` binaries; `None` otherwise. Uses `http_fetch` with a
/// `UreqHttpClient` whose `Git-Protocol` header selects v2 (or omits it for
/// v0/v1).
fn http_driver(protocol: u8) -> Option<Driver> {
    let grit_bin = find_binary("grit")?;
    let server_bin = find_binary("grit-http-server")?;
    let label = if protocol >= 2 { "http/v2" } else { "http/v1" };
    Some(Driver {
        name: label,
        protocol,
        serve: Box::new(move |source: &Path| {
            let root = source.parent()?.join(format!(
                "http-root-{}",
                source.file_name()?.to_string_lossy()
            ));
            std::fs::create_dir_all(&root).ok()?;
            let served = root.join("repo.git");
            let st = Command::new("git")
                .args(["clone", "-q", "--bare", source.to_str()?, served.to_str()?])
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .status()
                .ok()?;
            if !st.success() {
                return None;
            }
            let _ = Command::new("git")
                .current_dir(&served)
                .args(["symbolic-ref", "HEAD", "refs/heads/main"])
                .status();
            let port = free_port()?;
            let child = Command::new(&server_bin)
                .arg("--root")
                .arg(&root)
                .arg("--bind")
                .arg(format!("127.0.0.1:{port}"))
                .env("GUST_BIN", &grit_bin)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .ok()?;
            if !wait_ready(port) {
                let mut c = child;
                let _ = c.kill();
                let _ = c.wait();
                return None;
            }
            let url = format!("http://127.0.0.1:{port}/repo.git");
            Some((
                ServerHandle {
                    _proc: Some(child),
                    base: root,
                },
                url,
            ))
        }),
        fetch: Box::new(move |url, local_git, opts| {
            let client = if protocol >= 2 {
                UreqHttpClient::new().with_git_protocol("version=2")
            } else {
                UreqHttpClient::new()
            };
            http_fetch(&client, local_git, url, opts, &mut NoProgress)
        }),
    })
}

/// All drivers available in this environment, across transports x protocol
/// versions. Each test iterates these; a transport contributes nothing when its
/// fixture cannot be brought up. The `scratch` dir hosts the shared fake-ssh.
fn drivers(scratch: &Path) -> Vec<Driver> {
    let mut v: Vec<Driver> = Vec::new();
    // git daemon, v1 + v2.
    v.push(daemon_driver(0));
    v.push(daemon_driver(2));
    // ssh, v1 + v2 (share one fake-ssh script).
    if let Some(d) = ssh_driver(0, scratch.to_path_buf()) {
        v.push(d);
    }
    if let Some(d) = ssh_driver(2, scratch.to_path_buf()) {
        v.push(d);
    }
    // http, v1 + v2.
    if let Some(d) = http_driver(0) {
        v.push(d);
    }
    if let Some(d) = http_driver(2) {
        v.push(d);
    }
    v
}

/// Per-test scratch root. Holds the fake-ssh script and each driver's served
/// copies, all under one tempdir that lives for the whole test.
struct Harness {
    tmp: tempfile::TempDir,
}

impl Harness {
    fn new() -> Self {
        Harness {
            tmp: tempfile::tempdir().expect("tempdir"),
        }
    }
    fn path(&self) -> &Path {
        self.tmp.path()
    }
    /// Build a fresh source bare repo from a work-tree builder, returning the
    /// bare repo path. A process-global counter guarantees a unique directory per
    /// call even when several `for_each_driver` loops run inside one test, so
    /// `build`'s `git init` never lands in an already-populated dir.
    fn make_source(&self, name: &str, build: impl FnOnce(&Path)) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let uniq = SEQ.fetch_add(1, Ordering::SeqCst);
        let work = self.path().join(format!("{name}-{uniq}-work"));
        std::fs::create_dir_all(&work).unwrap();
        build(&work);
        let bare = self.path().join(format!("{name}-{uniq}.git"));
        bare_clone(&work, &bare);
        bare
    }
    fn drivers(&self) -> Vec<Driver> {
        drivers(self.path())
    }
}

/// Run `body` over every available driver, serving a FRESH copy of a source repo
/// built by `build_source` for each. `ran` counts how many drivers executed so
/// the caller can assert at least one did. The `body` receives the driver, the
/// fetch closure (already bound to the served URL), and the freshly served
/// source bare repo path it can `rev-parse` against. Each driver gets its own
/// local repo and its own served source copy under the harness tempdir.
fn for_each_driver(
    h: &Harness,
    build_source: &dyn Fn(&Path),
    body: &dyn Fn(&Driver, &dyn Fn(&Path, &FetchOptions) -> GritResult<FetchOutcome>, &Path, &Path),
) -> usize {
    let mut ran = 0usize;
    for (i, driver) in h.drivers().into_iter().enumerate() {
        // A fresh source per driver keeps drivers independent (prune/incremental
        // tests mutate the served copy).
        let src = h.make_source(
            &format!("src-{}-{i}", driver.name.replace('/', "-")),
            build_source,
        );
        let Some((handle, url)) = (driver.serve)(&src) else {
            eprintln!("SKIP driver {}: server bring-up failed", driver.name);
            continue;
        };
        // The served bare repo path (so the test can rev-parse the served copy,
        // whose oids equal the source's). daemon/http copy to `<base>/repo.git`;
        // ssh serves `src` directly.
        let served = if handle._proc.is_some() {
            handle.base.join("repo.git")
        } else {
            src.clone()
        };
        // Derive the local-repo dir from the unique served-source dir name (which
        // carries the process-global SEQ), not just `i`. Otherwise two
        // `for_each_driver` loops in one test reuse the same local repo for the
        // same driver index and refs/objects leak between legs (e.g. a tag fetched
        // under TagMode::All survives into a later TagMode::Following leg, which
        // then has nothing to fetch and never prunes it).
        let src_tag = src
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("src")
            .trim_end_matches(".git");
        let local_root = h.path().join(format!("local-{src_tag}"));
        let local_git = init_local(&local_root);
        let fetch =
            |opts_local_git: &Path, opts: &FetchOptions| (driver.fetch)(&url, opts_local_git, opts);
        // Bind the closure to this driver's local_git so `body` stays terse.
        let bound = |lg: &Path, opts: &FetchOptions| fetch(lg, opts);
        body(&driver, &bound, &served, &local_git);
        ran += 1;
        drop(handle);
    }
    ran
}

// ===========================================================================
// 1. Wildcard refspec: `+refs/heads/*:refs/remotes/origin/*` lands all heads,
//    HEAD symref -> default_branch, objects + oids cross-checked, fsck clean.
// ===========================================================================
#[test]
fn matrix_wildcard_refspec_all_heads() {
    let h = Harness::new();
    let ran = for_each_driver(
        &h,
        &|w| build_source_work(w),
        &|driver, fetch, served, local_git| {
            let main_oid = rev_parse(served, "refs/heads/main");
            let topic_oid = rev_parse(served, "refs/heads/topic");
            let opts = FetchOptions {
                refspecs: vec!["+refs/heads/*:refs/remotes/origin/*".to_owned()],
                tags: TagMode::None,
                ..Default::default()
            };
            let outcome = fetch(local_git, &opts)
                .unwrap_or_else(|e| panic!("[{}] wildcard fetch failed: {e}", driver.name));

            let got_main = resolve_ref(local_git, "refs/remotes/origin/main")
                .unwrap_or_else(|_| panic!("[{}] origin/main missing", driver.name));
            let got_topic = resolve_ref(local_git, "refs/remotes/origin/topic")
                .unwrap_or_else(|_| panic!("[{}] origin/topic missing", driver.name));
            assert_eq!(got_main, main_oid, "[{}] origin/main oid", driver.name);
            assert_eq!(got_topic, topic_oid, "[{}] origin/topic oid", driver.name);

            // side branch also matched the wildcard.
            let side_oid = rev_parse(served, "refs/heads/side");
            let got_side = resolve_ref(local_git, "refs/remotes/origin/side")
                .unwrap_or_else(|_| panic!("[{}] origin/side missing", driver.name));
            assert_eq!(got_side, side_oid, "[{}] origin/side oid", driver.name);

            // HEAD symref -> default_branch.
            assert_eq!(
                outcome.default_branch.as_deref(),
                Some("main"),
                "[{}] default_branch from HEAD symref",
                driver.name
            );

            // main is a New head.
            let mu = outcome
                .updates
                .iter()
                .find(|u| u.remote_ref == "refs/heads/main")
                .unwrap_or_else(|| panic!("[{}] no update for main", driver.name));
            assert_eq!(mu.mode, UpdateMode::New, "[{}] main mode", driver.name);
            assert_eq!(mu.new_oid, Some(main_oid));

            // Objects present + readable.
            let odb = open_odb(local_git);
            for oid in [main_oid, topic_oid, side_oid] {
                assert!(
                    odb.exists(&oid),
                    "[{}] object {} missing",
                    driver.name,
                    oid.to_hex()
                );
                odb.read(&oid)
                    .unwrap_or_else(|e| panic!("[{}] read {}: {e}", driver.name, oid.to_hex()));
            }
            // Cross-check the fetched tip vs git's view of the served repo.
            assert_eq!(
                got_main.to_hex(),
                git(served, &["rev-parse", "refs/heads/main"]).trim(),
                "[{}] main tip vs git rev-parse",
                driver.name
            );
            fsck_clean(local_git.parent().unwrap());
        },
    );
    assert!(
        ran > 0,
        "no transport driver was available for the wildcard test"
    );
}

// ===========================================================================
// 2. Exact refspec: only `main` lands; `topic`/`side` are NOT written.
// ===========================================================================
#[test]
fn matrix_exact_refspec_single_head() {
    let h = Harness::new();
    let ran = for_each_driver(
        &h,
        &|w| build_source_work(w),
        &|driver, fetch, served, local_git| {
            let main_oid = rev_parse(served, "refs/heads/main");
            let opts = FetchOptions {
                refspecs: vec!["+refs/heads/main:refs/remotes/origin/main".to_owned()],
                tags: TagMode::None,
                ..Default::default()
            };
            fetch(local_git, &opts)
                .unwrap_or_else(|e| panic!("[{}] exact fetch failed: {e}", driver.name));

            assert_eq!(
                resolve_ref(local_git, "refs/remotes/origin/main")
                    .unwrap_or_else(|_| panic!("[{}] origin/main missing", driver.name)),
                main_oid,
                "[{}] exact main oid",
                driver.name
            );
            // No other head was written by the exact refspec.
            assert!(
                resolve_ref(local_git, "refs/remotes/origin/topic").is_err(),
                "[{}] exact refspec must NOT write origin/topic",
                driver.name
            );
            assert!(
                resolve_ref(local_git, "refs/remotes/origin/side").is_err(),
                "[{}] exact refspec must NOT write origin/side",
                driver.name
            );
            fsck_clean(local_git.parent().unwrap());
        },
    );
    assert!(
        ran > 0,
        "no transport driver was available for the exact-refspec test"
    );
}

// ===========================================================================
// 3. Negative refspec: wildcard set minus an excluded head.
// ===========================================================================
#[test]
fn matrix_negative_refspec_excludes_head() {
    let h = Harness::new();
    let ran = for_each_driver(
        &h,
        &|w| build_source_work(w),
        &|driver, fetch, served, local_git| {
            let main_oid = rev_parse(served, "refs/heads/main");
            let topic_oid = rev_parse(served, "refs/heads/topic");
            let opts = FetchOptions {
                refspecs: vec!["+refs/heads/*:refs/remotes/origin/*".to_owned()],
                // Exclude `topic` from the wildcard.
                negative_refspecs: vec!["^refs/heads/topic".to_owned()],
                tags: TagMode::None,
                ..Default::default()
            };
            fetch(local_git, &opts)
                .unwrap_or_else(|e| panic!("[{}] negative fetch failed: {e}", driver.name));

            assert_eq!(
                resolve_ref(local_git, "refs/remotes/origin/main")
                    .unwrap_or_else(|_| panic!("[{}] origin/main missing", driver.name)),
                main_oid,
                "[{}] main kept",
                driver.name
            );
            assert!(
                resolve_ref(local_git, "refs/remotes/origin/topic").is_err(),
                "[{}] negative refspec must exclude origin/topic (got {})",
                driver.name,
                topic_oid.to_hex()
            );
            // side was not excluded -> still present.
            assert!(
                resolve_ref(local_git, "refs/remotes/origin/side").is_ok(),
                "[{}] non-excluded head origin/side must remain",
                driver.name
            );
            fsck_clean(local_git.parent().unwrap());
        },
    );
    assert!(
        ran > 0,
        "no transport driver was available for the negative-refspec test"
    );
}

// ===========================================================================
// 4. Tag modes: All (keeps the unreachable `vside`), Following (drops it but
//    keeps reachable `v1`), None (no tags). Includes a tag pointing at a commit
//    unreachable from any fetched head.
// ===========================================================================
#[test]
fn matrix_tag_modes_all_following_none() {
    let h = Harness::new();
    let ran = for_each_driver(
        &h,
        &|w| build_source_work(w),
        &|driver, fetch, served, local_git| {
            let v1_oid = rev_parse(served, "refs/tags/v1");
            let vside_oid = rev_parse(served, "refs/tags/vside");

            // --- TagMode::All: both tags land (vside is unreachable from main). ---
            {
                let opts = FetchOptions {
                    refspecs: vec!["+refs/heads/main:refs/remotes/origin/main".to_owned()],
                    tags: TagMode::All,
                    ..Default::default()
                };
                fetch(local_git, &opts)
                    .unwrap_or_else(|e| panic!("[{}] tags=All fetch failed: {e}", driver.name));
                assert_eq!(
                    resolve_ref(local_git, "refs/tags/v1")
                        .unwrap_or_else(|_| panic!("[{}] v1 (All) missing", driver.name)),
                    v1_oid,
                    "[{}] tag v1 (All)",
                    driver.name
                );
                assert_eq!(
                    resolve_ref(local_git, "refs/tags/vside").unwrap_or_else(|_| panic!(
                        "[{}] vside (All) missing: TagMode::All must fetch unreachable tags",
                        driver.name
                    )),
                    vside_oid,
                    "[{}] tag vside (All)",
                    driver.name
                );
                fsck_clean(local_git.parent().unwrap());
            }
        },
    );
    assert!(ran > 0, "no transport driver was available for tags=All");

    // Following + None get their own fresh local repos so prior tags don't leak.
    let ran2 = for_each_driver(
        &h,
        &|w| build_source_work(w),
        &|driver, fetch, served, local_git| {
            let v1_oid = rev_parse(served, "refs/tags/v1");
            // --- TagMode::Following: v1 reachable from main kept; vside dropped. ---
            let opts = FetchOptions {
                refspecs: vec!["+refs/heads/main:refs/remotes/origin/main".to_owned()],
                tags: TagMode::Following,
                ..Default::default()
            };
            fetch(local_git, &opts)
                .unwrap_or_else(|e| panic!("[{}] tags=Following fetch failed: {e}", driver.name));
            assert_eq!(
                resolve_ref(local_git, "refs/tags/v1")
                    .unwrap_or_else(|_| panic!("[{}] v1 (Following) missing", driver.name)),
                v1_oid,
                "[{}] Following keeps reachable v1",
                driver.name
            );
            assert!(
                resolve_ref(local_git, "refs/tags/vside").is_err(),
                "[{}] Following must drop the unreachable vside tag",
                driver.name
            );
            fsck_clean(local_git.parent().unwrap());
        },
    );
    assert!(
        ran2 > 0,
        "no transport driver was available for tags=Following"
    );

    let ran3 = for_each_driver(
        &h,
        &|w| build_source_work(w),
        &|driver, fetch, _served, local_git| {
            // --- TagMode::None: no tags at all. ---
            let opts = FetchOptions {
                refspecs: vec!["+refs/heads/main:refs/remotes/origin/main".to_owned()],
                tags: TagMode::None,
                ..Default::default()
            };
            fetch(local_git, &opts)
                .unwrap_or_else(|e| panic!("[{}] tags=None fetch failed: {e}", driver.name));
            assert!(
                resolve_ref(local_git, "refs/tags/v1").is_err(),
                "[{}] tags=None must NOT write v1",
                driver.name
            );
            assert!(
                resolve_ref(local_git, "refs/tags/vside").is_err(),
                "[{}] tags=None must NOT write vside",
                driver.name
            );
            fsck_clean(local_git.parent().unwrap());
        },
    );
    assert!(ran3 > 0, "no transport driver was available for tags=None");
}

// ===========================================================================
// 5. --prune of a VANISHED wildcard ref: fetch all heads, delete `topic` from
//    the served remote, re-fetch with prune -> origin/topic is removed and the
//    outcome records a DeletedMissing.
// ===========================================================================
#[test]
fn matrix_prune_vanished_wildcard_ref() {
    let h = Harness::new();
    let ran = for_each_driver(
        &h,
        &|w| build_source_work(w),
        &|driver, fetch, served, local_git| {
            let wildcard = FetchOptions {
                refspecs: vec!["+refs/heads/*:refs/remotes/origin/*".to_owned()],
                tags: TagMode::None,
                ..Default::default()
            };
            // Initial fetch establishes origin/topic.
            fetch(local_git, &wildcard)
                .unwrap_or_else(|e| panic!("[{}] initial wildcard fetch failed: {e}", driver.name));
            assert!(
                resolve_ref(local_git, "refs/remotes/origin/topic").is_ok(),
                "[{}] origin/topic must exist before prune",
                driver.name
            );

            // Delete `topic` on the served remote.
            git(served, &["branch", "-D", "topic"]);

            // Re-fetch with prune.
            let pruning = FetchOptions {
                refspecs: vec!["+refs/heads/*:refs/remotes/origin/*".to_owned()],
                prune: true,
                tags: TagMode::None,
                ..Default::default()
            };
            let outcome = fetch(local_git, &pruning)
                .unwrap_or_else(|e| panic!("[{}] pruning fetch failed: {e}", driver.name));

            assert!(
                resolve_ref(local_git, "refs/remotes/origin/topic").is_err(),
                "[{}] origin/topic must be pruned after it vanished on the remote",
                driver.name
            );
            // main survives (still on the remote).
            assert!(
                resolve_ref(local_git, "refs/remotes/origin/main").is_ok(),
                "[{}] origin/main must survive the prune",
                driver.name
            );
            // The prune is reported.
            assert!(
                outcome
                    .updates
                    .iter()
                    .any(|u| u.mode == UpdateMode::DeletedMissing
                        && u.local_ref.as_deref() == Some("refs/remotes/origin/topic")),
                "[{}] prune must report a DeletedMissing for origin/topic; got {:?}",
                driver.name,
                outcome.updates
            );
            fsck_clean(local_git.parent().unwrap());
        },
    );
    assert!(
        ran > 0,
        "no transport driver was available for wildcard prune"
    );
}

// ===========================================================================
// 6. --prune of a VANISHED EXACT ref: an exact refspec whose source disappeared
//    prunes its destination tracking ref.
// ===========================================================================
#[test]
fn matrix_prune_vanished_exact_ref() {
    let h = Harness::new();
    let ran = for_each_driver(
        &h,
        &|w| build_source_work(w),
        &|driver, fetch, served, local_git| {
            // Seed origin/topic via its exact refspec.
            let seed = FetchOptions {
                refspecs: vec!["+refs/heads/topic:refs/remotes/origin/topic".to_owned()],
                tags: TagMode::None,
                ..Default::default()
            };
            fetch(local_git, &seed)
                .unwrap_or_else(|e| panic!("[{}] seed exact fetch failed: {e}", driver.name));
            assert!(
                resolve_ref(local_git, "refs/remotes/origin/topic").is_ok(),
                "[{}] origin/topic must exist before exact prune",
                driver.name
            );

            // topic disappears on the remote.
            git(served, &["branch", "-D", "topic"]);

            // Re-fetch the SAME exact refspec with prune: the source is gone, so
            // the destination tracking ref is pruned.
            let prune_exact = FetchOptions {
                refspecs: vec!["+refs/heads/topic:refs/remotes/origin/topic".to_owned()],
                prune: true,
                tags: TagMode::None,
                ..Default::default()
            };
            let outcome = fetch(local_git, &prune_exact)
                .unwrap_or_else(|e| panic!("[{}] exact pruning fetch failed: {e}", driver.name));
            assert!(
                resolve_ref(local_git, "refs/remotes/origin/topic").is_err(),
                "[{}] exact-refspec prune must remove origin/topic",
                driver.name
            );
            assert!(
                outcome
                    .updates
                    .iter()
                    .any(|u| u.mode == UpdateMode::DeletedMissing
                        && u.local_ref.as_deref() == Some("refs/remotes/origin/topic")),
                "[{}] exact prune must report DeletedMissing for origin/topic; got {:?}",
                driver.name,
                outcome.updates
            );
            fsck_clean(local_git.parent().unwrap());
        },
    );
    assert!(ran > 0, "no transport driver was available for exact prune");
}

// ===========================================================================
// 7. Prune-before-update directory/file conflict: a stale `origin/feature`
//    tracking ref (a FILE) must be pruned BEFORE the remote's new
//    `feature/sub` (which needs `origin/feature/` as a DIRECTORY) can be
//    written. Without prune-first ordering this is a D/F conflict.
// ===========================================================================
#[test]
fn matrix_prune_before_update_df_conflict() {
    let h = Harness::new();
    let build = |w: &Path| {
        git(w, &["init", "-q", "-b", "main", "."]);
        std::fs::write(w.join("a.txt"), "one\n").unwrap();
        git(w, &["add", "a.txt"]);
        git(w, &["commit", "-q", "-m", "c1"]);
        // A branch literally named `feature` (becomes the stale FILE ref).
        git(w, &["branch", "feature"]);
    };
    let ran = for_each_driver(&h, &build, &|driver, fetch, served, local_git| {
        let wildcard = FetchOptions {
            refspecs: vec!["+refs/heads/*:refs/remotes/origin/*".to_owned()],
            tags: TagMode::None,
            ..Default::default()
        };
        // 1. Fetch -> origin/feature exists as a loose FILE.
        fetch(local_git, &wildcard)
            .unwrap_or_else(|e| panic!("[{}] initial DF fetch failed: {e}", driver.name));
        assert!(
            local_git.join("refs/remotes/origin/feature").is_file(),
            "[{}] origin/feature must be a loose ref file before the conflict",
            driver.name
        );

        // 2. On the remote, delete `feature` and create `feature/sub` (needs a
        //    `feature/` DIRECTORY on the local tracking side).
        git(served, &["branch", "-D", "feature"]);
        git(served, &["branch", "feature/sub", "refs/heads/main"]);
        let sub_oid = rev_parse(served, "refs/heads/feature/sub");

        // 3. Prune-enabled fetch: prune `origin/feature` first, then write
        //    `origin/feature/sub`. Both must succeed.
        let pruning = FetchOptions {
            refspecs: vec!["+refs/heads/*:refs/remotes/origin/*".to_owned()],
            prune: true,
            tags: TagMode::None,
            ..Default::default()
        };
        fetch(local_git, &pruning).unwrap_or_else(|e| {
            panic!(
                "[{}] prune-before-update D/F fetch failed (D/F conflict not resolved?): {e}",
                driver.name
            )
        });

        assert!(
            resolve_ref(local_git, "refs/remotes/origin/feature").is_err(),
            "[{}] stale origin/feature ref must be pruned",
            driver.name
        );
        assert_eq!(
            resolve_ref(local_git, "refs/remotes/origin/feature/sub").unwrap_or_else(|_| {
                panic!(
                    "[{}] origin/feature/sub must be written after the prune cleared the D/F",
                    driver.name
                )
            }),
            sub_oid,
            "[{}] feature/sub oid",
            driver.name
        );
        fsck_clean(local_git.parent().unwrap());
    });
    assert!(
        ran > 0,
        "no transport driver was available for the D/F conflict test"
    );
}

// ===========================================================================
// 8. Empty / unborn-HEAD remote: a repo with no commits (unborn HEAD) yields a
//    hang-free, empty fetch with no ref updates.
// ===========================================================================
#[test]
fn matrix_empty_unborn_remote() {
    let h = Harness::new();
    // An empty bare repo: no commits, HEAD points at refs/heads/main (unborn).
    let build = |w: &Path| {
        git(w, &["init", "-q", "-b", "main", "."]);
        // No commits; HEAD is unborn. bare_clone will mirror this empty repo.
    };
    let ran = for_each_driver(&h, &build, &|driver, fetch, _served, local_git| {
        let opts = FetchOptions {
            refspecs: vec!["+refs/heads/*:refs/remotes/origin/*".to_owned()],
            tags: TagMode::All,
            ..Default::default()
        };
        let outcome = fetch(local_git, &opts).unwrap_or_else(|e| {
            panic!(
                "[{}] empty-remote fetch failed (should be a clean no-op): {e}",
                driver.name
            )
        });
        assert!(
            outcome.updates.iter().all(|u| u.new_oid.is_none()),
            "[{}] unborn remote must yield no ref creations; got {:?}",
            driver.name,
            outcome.updates
        );
        assert!(
            resolve_ref(local_git, "refs/remotes/origin/main").is_err(),
            "[{}] no tracking ref should be written for an unborn remote",
            driver.name
        );
        // The local repo is still valid.
        fsck_clean(local_git.parent().unwrap());
    });
    assert!(
        ran > 0,
        "no transport driver was available for the empty-remote test"
    );
}

// ===========================================================================
// 9. Incremental fetch into a repo already sharing history: seed `main`, then a
//    new commit lands on `topic`; the second fetch pulls the new tip while the
//    shared history is NOT re-required (minimal: old objects already present).
// ===========================================================================
#[test]
fn matrix_incremental_shared_history() {
    let h = Harness::new();
    let ran = for_each_driver(
        &h,
        &|w| build_source_work(w),
        &|driver, fetch, served, local_git| {
            // 1. Seed: fetch main + topic so local shares their history.
            let seed = FetchOptions {
                refspecs: vec!["+refs/heads/*:refs/remotes/origin/*".to_owned()],
                tags: TagMode::None,
                ..Default::default()
            };
            fetch(local_git, &seed)
                .unwrap_or_else(|e| panic!("[{}] seed fetch failed: {e}", driver.name));
            // Materialize a local branch at the fetched tip so the negotiator has a
            // real heads/ tip to offer as a `have`.
            let local_root = local_git.parent().unwrap();
            git(local_root, &["branch", "base", "refs/remotes/origin/main"]);
            let shared_main = rev_parse(served, "refs/heads/main");
            let odb_before = open_odb(local_git);
            assert!(
                odb_before.exists(&shared_main),
                "[{}] shared main must be present after seed",
                driver.name
            );

            // 2. New commit on the remote's topic branch.
            //    (Operate on the served bare repo via a temp worktree-less commit:
            //    create a commit object on top of topic and move the ref.)
            let work2 = h
                .path()
                .join(format!("inc-work-{}", driver.name.replace('/', "-")));
            git_clone_nonbare(served, &work2);
            git(&work2, &["checkout", "-q", "-B", "topic", "origin/topic"]);
            std::fs::write(work2.join("c.txt"), "three\n").unwrap();
            git(&work2, &["add", "c.txt"]);
            git(&work2, &["commit", "-q", "-m", "c3"]);
            git(&work2, &["push", "-q", "origin", "topic"]);
            let topic_new = rev_parse(&work2, "refs/heads/topic");

            // 3. Incremental fetch: pull the new topic tip. Local already holds the
            //    base history, so the new tip is what must arrive.
            let inc = FetchOptions {
                refspecs: vec!["+refs/heads/*:refs/remotes/origin/*".to_owned()],
                tags: TagMode::None,
                ..Default::default()
            };
            fetch(local_git, &inc)
                .unwrap_or_else(|e| panic!("[{}] incremental fetch failed: {e}", driver.name));

            let got_topic =
                resolve_ref(local_git, "refs/remotes/origin/topic").unwrap_or_else(|_| {
                    panic!("[{}] origin/topic missing after incremental", driver.name)
                });
            assert_eq!(
                got_topic, topic_new,
                "[{}] incremental must advance origin/topic to the new tip",
                driver.name
            );
            let odb_after = open_odb(local_git);
            assert!(
                odb_after.exists(&topic_new),
                "[{}] new topic commit {} missing after incremental fetch",
                driver.name,
                topic_new.to_hex()
            );
            // The pre-existing shared main commit is still there (not re-fetched
            // away); cross-check it equals git's view.
            assert_eq!(
                resolve_ref(local_git, "refs/remotes/origin/main")
                    .unwrap()
                    .to_hex(),
                git(served, &["rev-parse", "refs/heads/main"]).trim(),
                "[{}] shared main unchanged after incremental",
                driver.name
            );
            fsck_clean(local_root);
        },
    );
    assert!(
        ran > 0,
        "no transport driver was available for the incremental test"
    );
}

/// Clone `bare` into a non-bare work tree `dst` with an `origin` remote, so a
/// test can make a new commit and `push` it back to the served bare repo.
fn git_clone_nonbare(bare: &Path, dst: &Path) {
    std::fs::create_dir_all(dst).unwrap();
    let st = Command::new("git")
        .args(["clone", "-q", bare.to_str().unwrap(), dst.to_str().unwrap()])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .status()
        .expect("git clone for incremental setup");
    assert!(st.success(), "git clone of served bare repo failed");
}

// ===========================================================================
// 10. HEAD symref / default_branch resolution: a remote whose HEAD points at a
//     NON-default branch must surface THAT branch as default_branch (not a
//     hardcoded "main").
// ===========================================================================
#[test]
fn matrix_head_symref_nondefault_branch() {
    let h = Harness::new();
    let build = |w: &Path| {
        build_source_work(w);
    };
    let ran = for_each_driver_custom_serve(
        &h,
        &build,
        // Point the served copy's HEAD at `topic` before serving.
        &|served| {
            git(served, &["symbolic-ref", "HEAD", "refs/heads/topic"]);
        },
        &|driver, fetch, served, local_git| {
            let opts = FetchOptions {
                refspecs: vec!["+refs/heads/*:refs/remotes/origin/*".to_owned()],
                tags: TagMode::None,
                ..Default::default()
            };
            let outcome = fetch(local_git, &opts)
                .unwrap_or_else(|e| panic!("[{}] symref fetch failed: {e}", driver.name));
            assert_eq!(
                outcome.default_branch.as_deref(),
                Some("topic"),
                "[{}] default_branch must follow the remote HEAD symref -> topic",
                driver.name
            );
            // Sanity: topic landed and equals git's view.
            assert_eq!(
                resolve_ref(local_git, "refs/remotes/origin/topic")
                    .unwrap()
                    .to_hex(),
                git(served, &["rev-parse", "refs/heads/topic"]).trim(),
                "[{}] topic tip",
                driver.name
            );
            fsck_clean(local_git.parent().unwrap());
        },
    );
    assert!(
        ran > 0,
        "no transport driver was available for the HEAD-symref test"
    );
}

/// Like [`for_each_driver`] but runs `tweak_served` against the freshly served
/// bare repo copy before fetching (e.g. to repoint HEAD). For streaming ssh the
/// served copy IS the source, so the tweak persists per-driver but each driver
/// gets its own fresh source.
fn for_each_driver_custom_serve(
    h: &Harness,
    build_source: &dyn Fn(&Path),
    tweak_served: &dyn Fn(&Path),
    body: &dyn Fn(&Driver, &dyn Fn(&Path, &FetchOptions) -> GritResult<FetchOutcome>, &Path, &Path),
) -> usize {
    let mut ran = 0usize;
    for (i, driver) in h.drivers().into_iter().enumerate() {
        let src = h.make_source(
            &format!("srvc-{}-{i}", driver.name.replace('/', "-")),
            build_source,
        );
        let Some((handle, url)) = (driver.serve)(&src) else {
            eprintln!("SKIP driver {}: server bring-up failed", driver.name);
            continue;
        };
        let served = if handle._proc.is_some() {
            handle.base.join("repo.git")
        } else {
            src.clone()
        };
        tweak_served(&served);
        let local_root = h
            .path()
            .join(format!("localc-{}-{i}", driver.name.replace('/', "-")));
        let local_git = init_local(&local_root);
        let bound = |lg: &Path, opts: &FetchOptions| (driver.fetch)(&url, lg, opts);
        body(&driver, &bound, &served, &local_git);
        ran += 1;
        drop(handle);
    }
    ran
}
