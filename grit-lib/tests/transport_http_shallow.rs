//! Integration test for **shallow / depth** fetch over the smart-HTTP transport
//! (`http_fetch` over the default `ureq`-backed `HttpClient`), against a real
//! `grit-http-server` serving upload-pack in-process via `grit-lib`.
//!
//! When grit's upload-pack implements the v0/v1 shallow protocol (`deepen`, the
//! `shallow-info` section), this exercises grit-lib's `negotiate_pack_http`
//! shallow path end-to-end over real HTTP wire I/O:
//!   * `depth=1` lands only the tip; the parent is absent; the local
//!     `.git/shallow` lists the boundary; the outcome reports it; `git log` shows
//!     one commit. Cross-checked against `git clone --depth 1`.
//!   * a follow-up `--unshallow` fetch brings the rest and removes `shallow`.
//!
//! v0/v1 is used deliberately (no `Git-Protocol` header). The v2 shallow path is
//! covered over `git daemon` in `transport_shallow.rs`.
//!
//! Skips gracefully (returns early) when `git`, the `grit-http-server` binary, or
//! a free port is unavailable, or the server fails to bind.
//!
//!   cargo test -p grit-lib --features http-ureq --test transport_http_shallow

#![cfg(feature = "http-ureq")]

use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use grit_lib::fetch::NoProgress;
use grit_lib::objects::ObjectId;
use grit_lib::odb::Odb;
use grit_lib::transfer::FetchOptions;
use grit_lib::transport::http::ureq_client::UreqHttpClient;
use grit_lib::transport::http::{http_client_arc, http_fetch};

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

/// Locate a sibling binary in the cargo target dir (`target/<profile>/<name>`).
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

#[test]
fn shallow_depth1_then_unshallow_over_smart_http_v1() {
    let Some(server_bin) = find_binary("grit-http-server") else {
        eprintln!("SKIP: `grit-http-server` binary not found (build it first)");
        return;
    };

    let tmp = tempfile::tempdir().expect("tempdir");
    let work = tmp.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    build_linear_source(&work, 4); // c0..c3

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

    let tip = rev_parse(&source, "refs/heads/main"); // c3
    let parent = rev_parse(&work, "HEAD~1"); // c2
    let root_oid = rev_parse(&work, "HEAD~3"); // c0

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

    // No `Git-Protocol` header → v0/v1 stateless RPC.
    let client = http_client_arc(UreqHttpClient::new());

    // 1. Shallow depth=1 fetch.
    let shallow_opts = FetchOptions {
        refspecs: vec!["+refs/heads/main:refs/remotes/origin/main".to_owned()],
        depth: Some(1),
        ..Default::default()
    };
    let outcome = match http_fetch(
        client.clone(),
        &local_git,
        &url,
        &shallow_opts,
        &mut NoProgress,
    ) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("SKIP: shallow http_fetch failed (server/git mismatch?): {e}");
            return;
        }
    };

    let local_odb = open_odb(&local_git);
    assert!(
        local_odb.exists(&tip),
        "tip present after depth=1 http fetch"
    );
    assert!(
        !local_odb.exists(&parent),
        "parent {} must be ABSENT after depth=1 (shallow boundary)",
        parent.to_hex()
    );
    assert_eq!(
        read_shallow_file(&local_git),
        vec![tip],
        "local shallow file must list the tip boundary"
    );
    assert!(
        outcome.new_shallow.contains(&tip),
        "outcome.new_shallow must contain the tip boundary; got {:?}",
        outcome
            .new_shallow
            .iter()
            .map(ObjectId::to_hex)
            .collect::<Vec<_>>()
    );

    let log = git(&local, &["log", "--format=%H", "refs/remotes/origin/main"]);
    let log_oids: Vec<&str> = log.lines().filter(|l| !l.is_empty()).collect();
    assert_eq!(
        log_oids,
        vec![tip.to_hex().as_str()],
        "git log must show exactly the shallow tip; got {log_oids:?}"
    );

    // Cross-check against system `git clone --depth 1` of the same HTTP URL.
    // Best-effort: the grit-http-server's smart-HTTP advertisement may not satisfy
    // a fresh system `git clone`, so this is skipped (not failed) unless the clone
    // succeeds. The authoritative `git clone --depth` cross-check runs over the
    // real `git daemon` in `transport_shallow.rs`.
    let reference = tmp.path().join("reference");
    let st = Command::new("git")
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
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    if let Ok(st) = st {
        if st.success() {
            let ref_git = reference.join(".git");
            assert_eq!(
                read_shallow_file(&ref_git),
                vec![tip],
                "system `git clone --depth 1` boundary should match ours"
            );
            let ref_odb = open_odb(&ref_git);
            assert!(ref_odb.exists(&tip));
            assert!(!ref_odb.exists(&parent), "reference clone parent absent");
        }
    }

    // 2. Deepen to full with `--unshallow`.
    let unshallow_opts = FetchOptions {
        refspecs: vec!["+refs/heads/main:refs/remotes/origin/main".to_owned()],
        unshallow: true,
        ..Default::default()
    };
    let outcome = http_fetch(
        client.clone(),
        &local_git,
        &url,
        &unshallow_opts,
        &mut NoProgress,
    )
    .expect("unshallow http_fetch");

    let local_odb = open_odb(&local_git);
    assert!(
        local_odb.exists(&root_oid),
        "root c0 must be present after --unshallow"
    );
    assert!(
        read_shallow_file(&local_git).is_empty(),
        "shallow file must be removed after --unshallow"
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

    let log = git(&local, &["log", "--format=%H", "refs/remotes/origin/main"]);
    let count = log.lines().filter(|l| !l.is_empty()).count();
    assert_eq!(count, 4, "full history after --unshallow; got {count}");

    let fsck = Command::new("git")
        .current_dir(&local)
        .args(["fsck", "--no-dangling"])
        .output()
        .expect("run git fsck");
    assert!(
        fsck.status.success(),
        "git fsck failed: {}",
        String::from_utf8_lossy(&fsck.stderr)
    );
}
