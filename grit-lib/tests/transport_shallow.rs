//! Integration tests for **shallow / depth** fetch over the `git://` streaming
//! transport (`GitDaemonTransport` + `fetch::fetch_remote`), against a real
//! `git daemon` (so the server is system `git upload-pack`, which fully
//! implements the shallow protocol).
//!
//! Coverage:
//!   * `depth=1` over protocol **v2**: only the tip commit lands, its parent is
//!     absent, the local `.git/shallow` lists the boundary, the outcome reports
//!     the new boundary, and `git -C <local> log` shows exactly one commit. We
//!     cross-check the shallow object set against a `git clone --depth 1`.
//!   * `depth=1` over protocol **v0/v1**: same assertions, proving the v0/v1
//!     `deepen`/shallow-info path works too.
//!   * **deepen to full** (`--unshallow`): a follow-up fetch with `unshallow`
//!     brings the rest of the history and removes the local `shallow` file, so
//!     the previously-absent parents are now present.
//!
//! Each test skips gracefully (returns early) when `git daemon` is unavailable
//! or fails to bind; the happy path is otherwise real end-to-end wire I/O.

use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use grit_lib::objects::odb::Odb;
use grit_lib::objects::ObjectId;
use grit_lib::progress::NeverCancel;
use grit_lib::remote::fetch::{fetch_remote, NoProgress};
use grit_lib::remote::transfer::{FetchDepth, FetchOptions};
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

/// Build a source repo with `n` linear commits on `main` (c0..c{n-1}).
fn build_linear_source(dir: &Path, n: usize) {
    git(dir, &["init", "-q", "-b", "main", "."]);
    for i in 0..n {
        std::fs::write(dir.join("f.txt"), format!("line {i}\n")).unwrap();
        git(dir, &["add", "f.txt"]);
        git(dir, &["commit", "-q", "-m", &format!("c{i}")]);
    }
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

fn spawn_daemon(base_path: &Path, port: u16) -> Option<Child> {
    Command::new("git")
        .arg("daemon")
        .arg("--listen=127.0.0.1")
        .arg(format!("--port={port}"))
        .arg("--reuseaddr")
        .arg("--export-all")
        .arg(format!("--base-path={}", base_path.display()))
        .arg(base_path)
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

/// Read the local `.git/shallow` file's boundary oids (empty when not shallow).
fn read_shallow_file(git_dir: &Path) -> Vec<ObjectId> {
    let p = git_dir.join("shallow");
    let Ok(s) = std::fs::read_to_string(&p) else {
        return Vec::new();
    };
    s.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .filter_map(|l| ObjectId::from_hex(l).ok())
        .collect()
}

/// Shared harness: build a 4-commit source, serve it, fetch `main` with
/// `depth=1` over the given protocol version, and assert the shallow invariants.
/// Returns nothing; panics on assertion failure, returns early (skip) when the
/// daemon is unavailable.
fn shallow_depth1_over_daemon(protocol_version: u8) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let work = tmp.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    build_linear_source(&work, 4); // c0..c3

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

    let tip = rev_parse(&source, "refs/heads/main"); // c3
    let parent = rev_parse(&work, "HEAD~1"); // c2

    let Some(port) = free_port() else {
        eprintln!("SKIP: could not allocate a free port");
        return;
    };
    let Some(child) = spawn_daemon(&base, port) else {
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

    let transport = GitDaemonTransport::new();
    let connect_opts = ConnectOptions {
        protocol_version,
        ..Default::default()
    };
    let mut conn = match transport.connect(&url, Service::UploadPack, &connect_opts) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("SKIP: could not connect to git daemon: {e}");
            return;
        }
    };

    let mut opts = FetchOptions {
        refspecs: vec!["+refs/heads/main:refs/remotes/origin/main".to_owned()],
        fetch_depth: FetchDepth::depth(1),
        ..Default::default()
    };
    let outcome = fetch_remote(
        &local_git,
        &mut *conn,
        &mut opts,
        &mut NoProgress,
        &NeverCancel,
    )
    .expect("shallow depth=1 fetch over git daemon");

    // The tip landed; its parent is absent (truncated history).
    let local_odb = open_odb(&local_git);
    assert!(
        local_odb.exists(&tip),
        "tip {} must be present after depth=1 fetch",
        tip.to_hex()
    );
    assert!(
        !local_odb.exists(&parent),
        "parent {} must be ABSENT after depth=1 fetch (shallow boundary)",
        parent.to_hex()
    );

    // The local `.git/shallow` lists exactly the tip as the boundary.
    let boundary = read_shallow_file(&local_git);
    assert_eq!(
        boundary,
        vec![tip],
        "local shallow file must list the tip {} as the only boundary; got {:?}",
        tip.to_hex(),
        boundary.iter().map(ObjectId::to_hex).collect::<Vec<_>>()
    );

    // The outcome surfaces the new boundary.
    assert!(
        outcome.new_shallow.contains(&tip),
        "FetchOutcome.new_shallow must contain the tip boundary; got {:?}",
        outcome
            .new_shallow
            .iter()
            .map(ObjectId::to_hex)
            .collect::<Vec<_>>()
    );
    assert!(
        outcome.new_unshallow.is_empty(),
        "a fresh shallow fetch must report no unshallow boundaries"
    );

    // `git log` in the local repo shows exactly one commit (depth=1).
    let log = git(&local, &["log", "--format=%H", "refs/remotes/origin/main"]);
    let log_oids: Vec<&str> = log.lines().filter(|l| !l.is_empty()).collect();
    assert_eq!(
        log_oids,
        vec![tip.to_hex().as_str()],
        "git log must show exactly the shallow tip; got {log_oids:?}"
    );

    // Cross-check: `git clone --depth 1` of the same source has the same shallow
    // boundary and the same object set (tip present, parent absent).
    let reference = tmp.path().join("reference");
    let status = Command::new("git")
        .args([
            "clone",
            "-q",
            "--depth",
            "1",
            &url,
            reference.to_str().expect("utf8"),
        ])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .status();
    if let Ok(st) = status {
        if st.success() {
            let ref_git = reference.join(".git");
            let ref_boundary = read_shallow_file(&ref_git);
            assert_eq!(
                ref_boundary,
                vec![tip],
                "system `git clone --depth 1` shallow boundary should match ours"
            );
            let ref_odb = open_odb(&ref_git);
            assert!(ref_odb.exists(&tip), "reference clone must have the tip");
            assert!(
                !ref_odb.exists(&parent),
                "reference clone must NOT have the truncated parent"
            );
        }
    }

    // `git fsck` is clean on the shallow repo.
    let fsck = Command::new("git")
        .current_dir(&local)
        .args(["fsck", "--no-dangling"])
        .output()
        .expect("run git fsck");
    assert!(
        fsck.status.success(),
        "git fsck failed on shallow repo: {}",
        String::from_utf8_lossy(&fsck.stderr)
    );
}

#[test]
fn shallow_depth1_fetch_over_git_daemon_v2() {
    shallow_depth1_over_daemon(2);
}

#[test]
fn shallow_depth1_fetch_over_git_daemon_v1() {
    shallow_depth1_over_daemon(0);
}

/// Shallow `depth=1`, then deepen to full history with `--unshallow`: the second
/// fetch brings the previously-absent ancestors and removes the `shallow` file.
#[test]
fn shallow_then_unshallow_over_git_daemon_v2() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let work = tmp.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    build_linear_source(&work, 4); // c0..c3

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

    let tip = rev_parse(&source, "refs/heads/main"); // c3
    let root = rev_parse(&work, "HEAD~3"); // c0

    let Some(port) = free_port() else {
        eprintln!("SKIP: could not allocate a free port");
        return;
    };
    let Some(child) = spawn_daemon(&base, port) else {
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

    let transport = GitDaemonTransport::new();
    let v2 = ConnectOptions {
        protocol_version: 2,
        ..Default::default()
    };

    // 1. Shallow depth=1 fetch.
    let mut conn = match transport.connect(&url, Service::UploadPack, &v2) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("SKIP: could not connect: {e}");
            return;
        }
    };
    let mut shallow_opts = FetchOptions {
        refspecs: vec!["+refs/heads/main:refs/remotes/origin/main".to_owned()],
        fetch_depth: FetchDepth::depth(1),
        ..Default::default()
    };
    fetch_remote(
        &local_git,
        &mut *conn,
        &mut shallow_opts,
        &mut NoProgress,
        &NeverCancel,
    )
    .expect("initial shallow fetch");
    drop(conn);

    let local_odb = open_odb(&local_git);
    assert!(local_odb.exists(&tip), "tip present after shallow fetch");
    assert!(
        !local_odb.exists(&root),
        "root c0 absent after depth=1 shallow fetch"
    );
    assert_eq!(
        read_shallow_file(&local_git),
        vec![tip],
        "shallow file lists the tip boundary after the depth=1 fetch"
    );

    // 2. Deepen to full with `--unshallow`.
    let mut conn = match transport.connect(&url, Service::UploadPack, &v2) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("SKIP: could not reconnect for unshallow: {e}");
            return;
        }
    };
    let mut unshallow_opts = FetchOptions {
        refspecs: vec!["+refs/heads/main:refs/remotes/origin/main".to_owned()],
        fetch_depth: Some(FetchDepth::Unshallow),
        ..Default::default()
    };
    let outcome = fetch_remote(
        &local_git,
        &mut *conn,
        &mut unshallow_opts,
        &mut NoProgress,
        &NeverCancel,
    )
    .expect("unshallow fetch");
    drop(conn);

    // The full history is now present and the shallow file is gone.
    let local_odb = open_odb(&local_git);
    assert!(
        local_odb.exists(&root),
        "root c0 must be present after --unshallow"
    );
    assert!(
        read_shallow_file(&local_git).is_empty(),
        "shallow file must be removed after --unshallow brings full history"
    );
    assert!(
        outcome.new_unshallow.contains(&tip),
        "unshallow fetch must report the tip as no-longer-shallow; got {:?}",
        outcome
            .new_unshallow
            .iter()
            .map(ObjectId::to_hex)
            .collect::<Vec<_>>()
    );

    // `git log` now shows all four commits.
    let log = git(&local, &["log", "--format=%H", "refs/remotes/origin/main"]);
    let count = log.lines().filter(|l| !l.is_empty()).count();
    assert_eq!(
        count, 4,
        "full history (4 commits) after --unshallow; got {count}"
    );

    let fsck = Command::new("git")
        .current_dir(&local)
        .args(["fsck", "--no-dangling"])
        .output()
        .expect("run git fsck");
    assert!(
        fsck.status.success(),
        "git fsck failed after unshallow: {}",
        String::from_utf8_lossy(&fsck.stderr)
    );
}

#[test]
fn git_daemon_shallow_depth_two_keeps_parent_tag() {
    use grit_lib::refs::resolve_ref;
    use grit_lib::remote::clone::{clone as clone_repo, CloneOptions};

    let tmp = tempfile::tempdir().expect("tempdir");
    let work = tmp.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    build_linear_source(&work, 3);
    let parent = rev_parse(&work, "HEAD~1");
    git(
        &work,
        &["tag", "-a", "parent-tag", "-m", "parent", &parent.to_hex()],
    );

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

    let Some(port) = free_port() else {
        eprintln!("SKIP: could not allocate a free port");
        return;
    };
    let Some(child) = spawn_daemon(&base, port) else {
        eprintln!("SKIP: `git daemon` is unavailable");
        return;
    };
    let _guard = DaemonGuard(child);
    if !wait_ready(port) {
        eprintln!("SKIP: git daemon not ready");
        return;
    }

    let url = format!("git://127.0.0.1:{port}/repo.git");
    let dest = tmp.path().join("clone");
    clone_repo(
        CloneOptions {
            depth: Some(2),
            ..CloneOptions::new(url, dest.clone())
        },
        &mut NoProgress,
        &NeverCancel,
        None,
    )
    .expect("git:// depth-2 clone");

    resolve_ref(&dest.join(".git"), "refs/tags/parent-tag")
        .expect("parent-tag after depth-2 clone over git://");
}
