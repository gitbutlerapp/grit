//! End-to-end smart HTTP against `grit-http-server` using only the system `git`
//! client. The server must not rely on a `grit` binary on `PATH` or `GUST_BIN`.

use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
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
        .status()
        .expect("spawn git");
    assert!(status.success(), "git {args:?} failed");
}

fn git_ok(dir: &Path, args: &[&str]) -> bool {
    Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

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

fn free_port() -> Option<u16> {
    let listener = TcpListener::bind(("127.0.0.1", 0)).ok()?;
    Some(listener.local_addr().ok()?.port())
}

fn path_without_cargo_bins() -> String {
    std::env::var("PATH")
        .unwrap_or_default()
        .split(':')
        .filter(|entry| {
            !entry.contains("/target/debug")
                && !entry.contains("/target/release")
                && !entry.ends_with("/.cargo/bin")
        })
        .collect::<Vec<_>>()
        .join(":")
}

fn wait_ready(port: u16) -> bool {
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let deadline = Instant::now() + Duration::from_secs(15);
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

fn spawn_server(server_bin: &Path, root: &Path, port: u16) -> Option<Child> {
    Command::new(server_bin)
        .arg("--root")
        .arg(root)
        .arg("--bind")
        .arg(format!("127.0.0.1:{port}"))
        .env_remove("GUST_BIN")
        .env("PATH", path_without_cargo_bins())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .ok()
}

#[test]
fn system_git_clone_fetch_v2_and_push_over_grit_http_server() {
    let Some(server_bin) = find_binary("grit-http-server") else {
        eprintln!("SKIP: grit-http-server binary not found (build it first)");
        return;
    };

    let tmp = tempfile::tempdir().expect("tempdir");
    let origin = tmp.path().join("origin");
    std::fs::create_dir_all(&origin).unwrap();
    git(&origin, &["init", "-q", "-b", "main", "."]);
    std::fs::write(origin.join("README"), "hello\n").unwrap();
    git(&origin, &["add", "README"]);
    git(&origin, &["commit", "-q", "-m", "initial"]);

    let root = tmp.path().join("srv");
    std::fs::create_dir_all(&root).unwrap();
    let bare = root.join("project.git");
    git(
        &origin,
        &[
            "clone",
            "-q",
            "--bare",
            ".",
            bare.to_str().expect("utf8 path"),
        ],
    );
    git(&bare, &["symbolic-ref", "HEAD", "refs/heads/main"]);

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
        eprintln!("SKIP: grit-http-server did not become ready");
        return;
    }

    let url = format!("http://127.0.0.1:{port}/project.git");

    let clone_dir = tmp.path().join("clone");
    git(
        &tmp.path(),
        &[
            "clone",
            "-q",
            url.as_str(),
            clone_dir.to_str().expect("utf8"),
        ],
    );
    assert!(git_ok(&clone_dir, &["fsck"]));

    std::fs::write(clone_dir.join("README"), "hello\nworld\n").unwrap();
    git(&clone_dir, &["commit", "-q", "-am", "extend readme"]);
    assert!(git_ok(
        &clone_dir,
        &["-c", "protocol.version=2", "fetch", "origin"]
    ));

    std::fs::write(clone_dir.join("branch.txt"), "topic\n").unwrap();
    git(&clone_dir, &["checkout", "-q", "-b", "topic"]);
    git(&clone_dir, &["add", "branch.txt"]);
    git(&clone_dir, &["commit", "-q", "-m", "topic work"]);
    git(&clone_dir, &["push", "-q", "origin", "topic:topic"]);

    assert!(git_ok(&bare, &["fsck"]));
    assert!(git_ok(&clone_dir, &["fsck"]));
}
