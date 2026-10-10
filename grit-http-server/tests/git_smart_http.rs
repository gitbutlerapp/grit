//! End-to-end smart HTTP against `grit-http-server` using only the system `git`
//! client. The server must not rely on a `grit` binary on `PATH` or `GUST_BIN`.

use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

fn git(dir: &Path, args: &[&str]) {
    let status = git_cmd(dir, args).status().expect("spawn git");
    assert!(status.success(), "git {args:?} failed");
}

fn git_cmd(dir: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new("git");
    cmd.current_dir(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_AUTHOR_DATE", "2005-04-07T22:13:13 +0200")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_DATE", "2005-04-07T22:13:13 +0200")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null");
    cmd
}

fn git_ok(dir: &Path, args: &[&str]) -> bool {
    git_cmd(dir, args)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn git_output(dir: &Path, args: &[&str]) -> Output {
    git_cmd(dir, args).output().expect("spawn git")
}

fn rev_parse(dir: &Path, rev: &str) -> String {
    let out = git_output(dir, &["rev-parse", rev]);
    assert!(out.status.success(), "git rev-parse {rev} failed");
    String::from_utf8(out.stdout)
        .expect("utf8")
        .trim()
        .to_owned()
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

fn require_binary(name: &str) -> PathBuf {
    find_binary(name).unwrap_or_else(|| {
        panic!("{name} binary not found (build grit-http-server with `cargo build -p grit-http-server`)")
    })
}

fn require_port() -> u16 {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind ephemeral port");
    listener.local_addr().expect("local addr").port()
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

fn spawn_server(server_bin: &Path, root: &Path, port: u16, home: Option<&Path>) -> Child {
    let mut cmd = Command::new(server_bin);
    cmd.arg("--root")
        .arg(root)
        .arg("--bind")
        .arg(format!("127.0.0.1:{port}"))
        .env_remove("GUST_BIN")
        .env("PATH", path_without_cargo_bins());
    if let Some(home) = home {
        cmd.env("HOME", home);
    }
    cmd.stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("could not spawn grit-http-server")
}

fn git_expect_failure(dir: &Path, args: &[&str]) {
    let status = git_cmd(dir, args).status().expect("spawn git");
    assert!(!status.success(), "git {args:?} should have failed");
}

#[test]
fn system_git_clone_fetch_v0_v2_and_push_over_grit_http_server() {
    let server_bin = require_binary("grit-http-server");

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

    let port = require_port();
    let child = spawn_server(&server_bin, &root, port, None);
    let _guard = ServerGuard(child);
    assert!(
        wait_ready(port),
        "grit-http-server did not become ready on port {port}"
    );

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

    // New commit on the served bare repo, then explicit protocol v0 fetch.
    std::fs::write(origin.join("v0-fetch.txt"), "v0\n").unwrap();
    git(&origin, &["add", "v0-fetch.txt"]);
    git(&origin, &["commit", "-q", "-m", "for v0 fetch"]);
    git(&origin, &["push", "-q", url.as_str(), "main"]);
    let v0_tip = rev_parse(&bare, "refs/heads/main");
    git(&clone_dir, &["-c", "protocol.version=0", "fetch", "origin"]);
    assert_eq!(
        rev_parse(&clone_dir, "origin/main"),
        v0_tip,
        "protocol v0 fetch must update origin/main to the remote tip"
    );

    // Another remote commit, then explicit protocol v2 fetch.
    std::fs::write(origin.join("v2-fetch.txt"), "v2\n").unwrap();
    git(&origin, &["add", "v2-fetch.txt"]);
    git(&origin, &["commit", "-q", "-m", "for v2 fetch"]);
    git(&origin, &["push", "-q", url.as_str(), "main"]);
    let v2_tip = rev_parse(&bare, "refs/heads/main");
    git(&clone_dir, &["-c", "protocol.version=2", "fetch", "origin"]);
    assert_eq!(
        rev_parse(&clone_dir, "origin/main"),
        v2_tip,
        "protocol v2 fetch must update origin/main to the remote tip"
    );

    std::fs::write(clone_dir.join("branch.txt"), "topic\n").unwrap();
    git(&clone_dir, &["checkout", "-q", "-b", "topic"]);
    git(&clone_dir, &["add", "branch.txt"]);
    git(&clone_dir, &["commit", "-q", "-m", "topic work"]);
    git(&clone_dir, &["push", "-q", "origin", "topic:topic"]);

    assert!(git_ok(&bare, &["fsck"]));
    assert!(git_ok(&clone_dir, &["fsck"]));
}

#[test]
fn global_receive_deny_non_fast_forward_rejects_force_push() {
    let server_bin = require_binary("grit-http-server");

    let tmp = tempfile::tempdir().expect("tempdir");
    let fake_home = tmp.path().join("home");
    std::fs::create_dir_all(&fake_home).unwrap();
    std::fs::write(
        fake_home.join(".gitconfig"),
        "[receive]\n\tdenyNonFastForwards = true\n",
    )
    .unwrap();

    let origin = tmp.path().join("origin");
    std::fs::create_dir_all(&origin).unwrap();
    git(&origin, &["init", "-q", "-b", "main", "."]);
    std::fs::write(origin.join("a.txt"), "1\n").unwrap();
    git(&origin, &["add", "a.txt"]);
    git(&origin, &["commit", "-q", "-m", "c1"]);
    std::fs::write(origin.join("b.txt"), "2\n").unwrap();
    git(&origin, &["add", "b.txt"]);
    git(&origin, &["commit", "-q", "-m", "c2"]);

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
    let advanced_tip = rev_parse(&bare, "refs/heads/main");

    let port = require_port();
    let child = spawn_server(&server_bin, &root, port, Some(&fake_home));
    let _guard = ServerGuard(child);
    assert!(
        wait_ready(port),
        "grit-http-server did not become ready on port {port}"
    );

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
    git(&clone_dir, &["reset", "--hard", "HEAD~1"]);
    git_expect_failure(&clone_dir, &["push", "--force", "origin", "main:main"]);
    assert_eq!(
        rev_parse(&bare, "refs/heads/main"),
        advanced_tip,
        "non-fast-forward force push must not rewind the served branch"
    );
}
