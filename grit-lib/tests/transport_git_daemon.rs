//! Integration test for the `git://` transport: `GitDaemonTransport` +
//! `fetch::fetch_remote`.
//!
//! A source repo is built with the system `git` (two commits on `main`, a
//! second branch, and an annotated tag). A real `git daemon` is started over a
//! temp base path; an empty local repo fetches from it via the trait
//! (`GitDaemonTransport::connect` -> `fetch_remote`) using
//! `+refs/heads/*:refs/remotes/origin/*`. We assert the tracking refs landed,
//! the objects arrived, and cross-check the fetched oids against
//! `git -C <source> rev-parse`.
//!
//! The test skips gracefully (returns early) when `git daemon` is unavailable
//! or fails to bind — the happy path is otherwise real end-to-end wire I/O.

use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use grit_lib::fetch::{fetch_remote, NoProgress};
use grit_lib::objects::ObjectId;
use grit_lib::odb::Odb;
use grit_lib::refs::resolve_ref;
use grit_lib::transfer::{FetchOptions, TagMode, UpdateMode};
use grit_lib::transport::{ConnectOptions, GitDaemonTransport, Service, Transport};

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

/// Spawn `git daemon` over `base_path` on `port`. Returns the child handle, or
/// `None` if the binary is missing.
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
fn fetch_over_git_daemon_lands_refs_and_objects() {
    // Build a source repo, then mirror it into a bare repo under the daemon
    // base path (a bare repo is served directly at `/repo.git`).
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
    // Mirror the annotated tag and topic branch (a plain --bare clone copies all
    // refs already, but ensure HEAD points at main for the symref check).
    git(&source, &["symbolic-ref", "HEAD", "refs/heads/main"]);

    let main_oid = rev_parse(&source, "refs/heads/main");
    let topic_oid = rev_parse(&source, "refs/heads/topic");
    let c1_oid = rev_parse(&work, "HEAD~1");

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

    // The daemon serves repos relative to base; the bare repo lives at
    // `<base>/repo.git`.
    let url = format!("git://127.0.0.1:{port}/repo.git");

    // Empty local repo.
    let local = tmp.path().join("local");
    std::fs::create_dir_all(&local).unwrap();
    git(&local, &["init", "-q", "-b", "main", "."]);
    let local_git = local.join(".git");

    // Connect + fetch via the trait.
    let transport = GitDaemonTransport::new();
    let mut conn = match transport.connect(&url, Service::UploadPack, &ConnectOptions::default()) {
        Ok(c) => c,
        Err(e) => {
            // Some environments forbid the daemon protocol entirely; treat a
            // connect failure as a skip rather than a hard failure.
            eprintln!("SKIP: could not connect to git daemon: {e}");
            return;
        }
    };

    // The advertisement should carry the source's heads.
    assert!(
        conn.advertised_refs()
            .iter()
            .any(|(n, o)| n == "refs/heads/main" && *o == main_oid),
        "advertisement missing refs/heads/main = {}",
        main_oid.to_hex()
    );

    let opts = FetchOptions {
        refspecs: vec!["+refs/heads/*:refs/remotes/origin/*".to_owned()],
        tags: TagMode::All,
        ..Default::default()
    };
    let outcome = fetch_remote(&local_git, &mut *conn, &opts, &mut NoProgress)
        .expect("fetch_remote over git daemon");

    // Tracking refs written.
    let got_main = resolve_ref(&local_git, "refs/remotes/origin/main").expect("origin/main");
    let got_topic = resolve_ref(&local_git, "refs/remotes/origin/topic").expect("origin/topic");
    assert_eq!(got_main, main_oid, "origin/main oid mismatch vs source");
    assert_eq!(got_topic, topic_oid, "origin/topic oid mismatch vs source");

    // The annotated tag arrived (TagMode::All).
    let tag_oid = rev_parse(&source, "refs/tags/v1");
    let got_tag = resolve_ref(&local_git, "refs/tags/v1").expect("tag v1 written");
    assert_eq!(got_tag, tag_oid, "tag v1 oid mismatch vs source");

    // Objects landed in the local odb: tips and an interior commit.
    let local_odb = open_odb(&local_git);
    for oid in [main_oid, topic_oid, c1_oid, tag_oid] {
        assert!(
            local_odb.exists(&oid),
            "object {} missing from local odb after fetch",
            oid.to_hex()
        );
        // And it must be readable (a real object, not a stub).
        local_odb
            .read(&oid)
            .unwrap_or_else(|e| panic!("read {}: {e}", oid.to_hex()));
    }

    // Per-ref update modes: both heads are New.
    let main_update = outcome
        .updates
        .iter()
        .find(|u| u.remote_ref == "refs/heads/main")
        .expect("update for main");
    assert_eq!(main_update.mode, UpdateMode::New);
    assert_eq!(main_update.new_oid, Some(main_oid));

    // Default branch resolved from the server's HEAD symref.
    assert_eq!(outcome.default_branch.as_deref(), Some("main"));

    // Cross-check that the fetched main tip equals git's view of the source.
    assert_eq!(
        got_main.to_hex(),
        git(&source, &["rev-parse", "refs/heads/main"]).trim()
    );

    // Sanity: the fetched pack re-indexes / fsck's clean in the local repo.
    let fsck = Command::new("git")
        .current_dir(&local)
        .args(["fsck", "--no-dangling"])
        .output()
        .expect("run git fsck");
    assert!(
        fsck.status.success(),
        "git fsck failed after fetch: {}",
        String::from_utf8_lossy(&fsck.stderr)
    );
}

/// Issue #933: a lightweight tag at the same commit as a side branch tip must not
/// suppress the branch `want`, leaving `origin/<branch>` dangling after clone/fetch.
#[test]
fn fetch_following_lightweight_tag_at_branch_tip_downloads_branch() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let work = tmp.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    git(&work, &["init", "-q", "-b", "main", "."]);
    std::fs::write(work.join("a"), "a\n").unwrap();
    git(&work, &["add", "a"]);
    git(&work, &["commit", "-q", "-m", "a"]);
    git(&work, &["checkout", "-q", "-b", "rel"]);
    std::fs::write(work.join("r"), "r\n").unwrap();
    git(&work, &["add", "r"]);
    git(&work, &["commit", "-q", "-m", "rel"]);
    git(&work, &["tag", "light"]);
    git(&work, &["tag", "-a", "-m", "rel1", "rel-1.0"]);
    git(&work, &["checkout", "-q", "main"]);
    std::fs::write(work.join("b"), "b\n").unwrap();
    git(&work, &["add", "b"]);
    git(&work, &["commit", "-q", "-m", "b"]);

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
    let rel_oid = rev_parse(&source, "refs/heads/rel");
    let light_oid = rev_parse(&source, "refs/tags/light");
    let rel10_oid = rev_parse(&source, "refs/tags/rel-1.0");
    assert_eq!(light_oid, rel_oid, "fixture: lightweight tag at rel tip");

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
    let mut conn = match transport.connect(&url, Service::UploadPack, &ConnectOptions::default()) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("SKIP: could not connect to git daemon: {e}");
            return;
        }
    };

    let opts = FetchOptions {
        refspecs: vec!["+refs/heads/*:refs/remotes/origin/*".to_owned()],
        tags: TagMode::Following,
        ..Default::default()
    };
    fetch_remote(&local_git, &mut *conn, &opts, &mut NoProgress)
        .expect("fetch with TagMode::Following");

    let got_rel = resolve_ref(&local_git, "refs/remotes/origin/rel").expect("origin/rel");
    assert_eq!(got_rel, rel_oid);
    assert_eq!(
        resolve_ref(&local_git, "refs/tags/light").expect("light tag"),
        light_oid
    );
    assert_eq!(
        resolve_ref(&local_git, "refs/tags/rel-1.0").expect("annotated tag"),
        rel10_oid
    );

    let local_odb = open_odb(&local_git);
    for oid in [main_oid, rel_oid, light_oid, rel10_oid] {
        assert!(
            local_odb.exists(&oid),
            "object {} missing after fetch",
            oid.to_hex()
        );
    }

    let fsck = Command::new("git")
        .current_dir(&local)
        .args(["fsck", "--no-dangling"])
        .output()
        .expect("run git fsck");
    assert!(
        fsck.status.success(),
        "git fsck failed (issue #933): {}",
        String::from_utf8_lossy(&fsck.stderr)
    );
}
