//! Clone an empty bare remote over protocol v2 (`git://` + system `git daemon`).

use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use grit_lib::remote::clone::{clone, CloneOptions};
use grit_lib::remote::fetch::NoProgress;

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn free_port() -> Option<u16> {
    static USED: std::sync::Mutex<Vec<u16>> = std::sync::Mutex::new(Vec::new());
    let mut used = USED.lock().unwrap_or_else(|e| e.into_inner());
    for _ in 0..200 {
        let listener = TcpListener::bind(("127.0.0.1", 0)).ok()?;
        let port = listener.local_addr().ok()?.port();
        drop(listener);
        if !used.contains(&port) {
            used.push(port);
            return Some(port);
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

#[test]
fn clone_empty_bare_trunk_over_git_daemon_v2() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let base = tmp.path().join("srv");
    std::fs::create_dir_all(&base).expect("mkdir srv");
    let bare = base.join("origin.git");
    git(
        tmp.path(),
        &[
            "init",
            "-q",
            "--bare",
            "-b",
            "trunk",
            bare.to_str().unwrap(),
        ],
    );

    let Some(port) = free_port() else {
        eprintln!("SKIP: could not allocate port");
        return;
    };
    let Some(child) = spawn_daemon(&base, port) else {
        eprintln!("SKIP: git daemon unavailable");
        return;
    };
    let _guard = DaemonGuard(child);
    if !wait_ready(port) {
        eprintln!("SKIP: daemon not ready");
        return;
    }

    let url = format!("git://127.0.0.1:{port}/origin.git");
    let grit_dest = tmp.path().join("grit-clone");
    let outcome = clone(
        CloneOptions::new(url.clone(), grit_dest.clone()),
        &mut NoProgress,
        None,
    )
    .expect("grit clone over git:// v2");
    assert_eq!(outcome.branch, "trunk");
    assert!(outcome.checkout_oid.is_zero());
    let grit_head = std::fs::read_to_string(grit_dest.join(".git/HEAD")).expect("grit HEAD");
    assert_eq!(grit_head.trim(), "ref: refs/heads/trunk");

    let git_dest = tmp.path().join("git-clone");
    let mut git_clone = Command::new("git")
        .args([
            "-c",
            "protocol.version=2",
            "clone",
            "-q",
            url.as_str(),
            git_dest.to_str().unwrap(),
        ])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("git clone");
    let status = git_clone.wait().expect("wait");
    assert!(status.success(), "git clone v2 failed");
    let git_head = std::fs::read_to_string(git_dest.join(".git/HEAD")).expect("git HEAD");
    assert_eq!(git_head.trim(), grit_head.trim());

    git(&grit_dest, &["fsck", "--strict"]);
}
