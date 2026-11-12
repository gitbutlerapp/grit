//! Successful `grit clone` must not echo URL credentials on stdout or stderr.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const GRIT: &str = env!("CARGO_BIN_EXE_grit");
const SECRET: &str = "SECRETTOKEN_clone_redact_test";
const USER: &str = "clone-user";

fn grit_http_server_bin() -> PathBuf {
    grit_test_support::grit_http_server_binary()
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("bind")
        .local_addr()
        .expect("addr")
        .port()
}

fn wait_tcp(port: u16) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        if std::net::TcpStream::connect(format!("127.0.0.1:{port}")).is_ok() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("HTTP server did not start on port {port}");
}

struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn git_in(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .status()
        .expect("git");
    assert!(status.success(), "git {:?} failed", args);
}

fn prepare_bare_repo(root: &Path) -> PathBuf {
    let wt = root.join("wt");
    std::fs::create_dir_all(&wt).unwrap();
    git_in(&wt, &["init", "-q", "-b", "main", "."]);
    std::fs::write(wt.join("README"), "hello\n").unwrap();
    git_in(&wt, &["add", "README"]);
    git_in(&wt, &["commit", "-q", "-m", "seed"]);
    let bare = root.join("repo.git");
    git_in(&wt, &["clone", "-q", "--bare", ".", bare.to_str().unwrap()]);
    bare
}

struct HttpRemote {
    _server: ChildGuard,
    port: u16,
    _tmp: tempfile::TempDir,
}

fn start_http_remote() -> HttpRemote {
    let tmp = tempfile::tempdir().expect("tempdir");
    let srv_root = tmp.path().join("srv");
    std::fs::create_dir_all(&srv_root).unwrap();
    let bare = prepare_bare_repo(tmp.path());
    std::fs::rename(bare, srv_root.join("repo.git")).unwrap();
    let port = free_port();
    let child = Command::new(grit_http_server_bin())
        .arg("--root")
        .arg(&srv_root)
        .arg("--bind")
        .arg(format!("127.0.0.1:{port}"))
        .env("GRIT_BIN", GRIT)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn grit-http-server");
    wait_tcp(port);
    HttpRemote {
        _server: ChildGuard(child),
        port,
        _tmp: tmp,
    }
}

fn clone_url(port: u16) -> String {
    format!("http://{USER}:{SECRET}@127.0.0.1:{port}/repo.git")
}

fn assert_redacted(combined: &str, port: u16) {
    assert!(
        !combined.contains(SECRET),
        "credential password leaked: {combined}"
    );
    assert!(
        !combined.contains(USER),
        "credential username leaked: {combined}"
    );
    let redacted = format!("http://127.0.0.1:{port}/repo.git");
    assert!(
        combined.contains(&redacted),
        "expected redacted URL {redacted} in output: {combined}"
    );
}

fn assert_stored_remote_keeps_credentials(dest: &Path, port: u16) {
    let config = std::fs::read_to_string(dest.join(".git/config")).expect("config");
    let expected = format!("http://{USER}:{SECRET}@127.0.0.1:{port}/repo.git");
    assert!(
        config.contains(&expected),
        "stored remote.url must keep caller credentials for fetch; config:\n{config}"
    );
}

#[test]
fn clone_json_and_markdown_redact_url_credentials() {
    let remote = start_http_remote();
    let base = tempfile::tempdir().expect("dest tempdir");
    let url = clone_url(remote.port);

    for (mode, dest_name) in [("--json", "json-clone"), ("--markdown", "md-clone")] {
        let dest = base.path().join(dest_name);
        let out = Command::new(GRIT)
            .args(["clone", mode, &url, dest.to_str().unwrap()])
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .expect("grit clone");
        assert!(
            out.status.success(),
            "grit clone {mode} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_redacted(&format!("{stdout}{stderr}"), remote.port);
        if mode == "--json" {
            let parsed: serde_json::Value =
                serde_json::from_slice(&out.stdout).expect("clone json");
            assert_eq!(
                parsed["url"],
                format!("http://127.0.0.1:{}/repo.git", remote.port)
            );
        }
        assert_stored_remote_keeps_credentials(&dest, remote.port);
    }
}

#[test]
#[cfg(unix)]
fn clone_tty_banner_redacts_url_credentials() {
    let remote = start_http_remote();
    let base = tempfile::tempdir().expect("dest tempdir");
    let dest = base.path().join("tty-clone");
    let url = clone_url(remote.port);
    let grit = std::fs::canonicalize(GRIT).expect("grit path");
    let shell_cmd = format!(
        "env GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_SYSTEM=/dev/null {} clone {} {}",
        grit.display(),
        url,
        dest.display()
    );
    let mut script = Command::new("script");
    // util-linux `script` takes the command via `-c` before the transcript file; BSD/macOS
    // `script` takes the transcript file followed by the command and its arguments.
    if cfg!(target_os = "linux") {
        script.args(["-q", "-c", &shell_cmd, "/dev/null"]);
    } else {
        script.args(["-q", "/dev/null", "sh", "-c", &shell_cmd]);
    }
    let out = script
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("script grit clone");
    assert!(
        out.status.success(),
        "grit clone under pseudo-tty failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert_redacted(&combined, remote.port);
    assert!(
        combined.contains("Cloning"),
        "expected TTY clone banner: {combined}"
    );
    assert_stored_remote_keeps_credentials(&dest, remote.port);
}
