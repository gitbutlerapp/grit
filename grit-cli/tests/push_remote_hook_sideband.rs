//! Push surfaces receive-pack hook stderr relayed on the side-band (issue #1937).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

const GRIT: &str = env!("CARGO_BIN_EXE_grit");

fn git(dir: Option<&Path>, args: &[&str]) {
    let mut cmd = Command::new("git");
    if let Some(d) = dir {
        cmd.current_dir(d);
    }
    let out = cmd
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {:?}: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
}

fn grit_http_server_bin() -> PathBuf {
    grit_test_support::grit_http_server_binary()
}

fn write_hook(repo: &Path, name: &str, body: &str) {
    let hook = repo.join("hooks").join(name);
    std::fs::create_dir_all(hook.parent().expect("hooks dir")).unwrap();
    std::fs::write(&hook, body).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&hook).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&hook, perms).unwrap();
    }
}

fn free_port() -> Option<u16> {
    TcpListener::bind("127.0.0.1:0")
        .ok()
        .and_then(|l| l.local_addr().ok().map(|a| a.port()))
}

fn wait_tcp(port: u16) -> bool {
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        if std::net::TcpStream::connect(format!("127.0.0.1:{port}")).is_ok() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

struct ChildGuard(std::process::Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
    }
}

#[test]
fn push_surfaces_pre_receive_hook_stderr_on_sideband() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let seed = tmp.path().join("seed");
    git(
        Some(tmp.path()),
        &["init", "-q", "-b", "main", seed.to_str().unwrap()],
    );
    git(Some(&seed), &["config", "user.name", "a"]);
    git(Some(&seed), &["config", "user.email", "a@b"]);
    std::fs::write(seed.join("f"), "1\n").unwrap();
    git(Some(&seed), &["add", "."]);
    git(Some(&seed), &["commit", "-qm", "c1"]);

    let bare = tmp.path().join("p.git");
    git(
        Some(tmp.path()),
        &[
            "clone",
            "-q",
            "--bare",
            seed.to_str().unwrap(),
            bare.to_str().unwrap(),
        ],
    );
    git(Some(&bare), &["config", "http.receivepack", "true"]);
    write_hook(
        &bare,
        "pre-receive",
        "#!/bin/sh\necho \"policy: commit message must reference a ticket\" >&2\nexit 1\n",
    );

    let srv = tmp.path().join("srv");
    std::fs::create_dir_all(&srv).unwrap();
    std::fs::rename(&bare, srv.join("p.git")).unwrap();

    let port = free_port().expect("port");
    let server_bin = grit_http_server_bin();
    let grit_bin = PathBuf::from(GRIT);
    let child = Command::new(&server_bin)
        .arg("--root")
        .arg(&srv)
        .arg("--bind")
        .arg(format!("127.0.0.1:{port}"))
        .env("GRIT_BIN", &grit_bin)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn server");
    let _guard = ChildGuard(child);
    assert!(wait_tcp(port), "grit-http-server did not listen");

    let work = tmp.path().join("w");
    let url = format!("http://127.0.0.1:{port}/p.git");
    git(
        Some(tmp.path()),
        &["clone", "-q", url.as_str(), work.to_str().unwrap()],
    );
    std::fs::write(work.join("x"), "x\n").unwrap();
    git(Some(&work), &["add", "x"]);
    git(Some(&work), &["commit", "-qm", "x"]);

    let human = Command::new(GRIT)
        .args(["push", "origin", "HEAD:new"])
        .current_dir(&work)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("grit push human");
    let human_err = String::from_utf8_lossy(&human.stderr);
    assert!(
        human_err.contains("remote: policy: commit message must reference a ticket"),
        "stderr missing hook line: {human_err}"
    );
    let human_out = String::from_utf8_lossy(&human.stdout);
    assert!(
        human_out.contains("pre-receive hook declined"),
        "stdout missing rejection: {human_out}"
    );

    let json_out = Command::new(GRIT)
        .args(["push", "--json", "origin", "HEAD:new"])
        .current_dir(&work)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("grit push json");
    let json: Value = serde_json::from_slice(&json_out.stdout).expect("json stdout");
    let messages = json["remote_messages"]
        .as_array()
        .expect("remote_messages array");
    assert!(
        messages
            .iter()
            .any(|m| m.as_str() == Some("policy: commit message must reference a ticket")),
        "json remote_messages: {json}"
    );
    assert_eq!(json["rejected"], Value::Bool(true));
}

#[test]
fn push_surfaces_post_receive_hook_stderr_on_success() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let seed = tmp.path().join("seed");
    git(
        Some(tmp.path()),
        &["init", "-q", "-b", "main", seed.to_str().unwrap()],
    );
    git(Some(&seed), &["config", "user.name", "a"]);
    git(Some(&seed), &["config", "user.email", "a@b"]);
    std::fs::write(seed.join("f"), "1\n").unwrap();
    git(Some(&seed), &["add", "."]);
    git(Some(&seed), &["commit", "-qm", "c1"]);

    let bare = tmp.path().join("p.git");
    git(
        Some(tmp.path()),
        &[
            "clone",
            "-q",
            "--bare",
            seed.to_str().unwrap(),
            bare.to_str().unwrap(),
        ],
    );
    git(Some(&bare), &["config", "http.receivepack", "true"]);
    write_hook(
        &bare,
        "post-receive",
        "#!/bin/sh\necho \"post-receive: build triggered https://ci.example/run/42\" >&2\n",
    );

    let srv = tmp.path().join("srv");
    std::fs::create_dir_all(&srv).unwrap();
    std::fs::rename(&bare, srv.join("p.git")).unwrap();

    let port = free_port().expect("port");
    let server_bin = grit_http_server_bin();
    let grit_bin = PathBuf::from(GRIT);
    let child = Command::new(&server_bin)
        .arg("--root")
        .arg(&srv)
        .arg("--bind")
        .arg(format!("127.0.0.1:{port}"))
        .env("GRIT_BIN", &grit_bin)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn server");
    let _guard = ChildGuard(child);
    assert!(wait_tcp(port));

    let work = tmp.path().join("w");
    let url = format!("http://127.0.0.1:{port}/p.git");
    git(
        Some(tmp.path()),
        &["clone", "-q", url.as_str(), work.to_str().unwrap()],
    );
    std::fs::write(work.join("x"), "x\n").unwrap();
    git(Some(&work), &["add", "x"]);
    git(Some(&work), &["commit", "-qm", "x"]);

    let human = Command::new(GRIT)
        .args(["push", "origin", "HEAD:okbranch"])
        .current_dir(&work)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("grit push");
    assert!(human.status.success(), "push failed: {:?}", human.stderr);
    let human_err = String::from_utf8_lossy(&human.stderr);
    assert!(
        human_err.contains("post-receive: build triggered https://ci.example/run/42"),
        "stderr missing post-receive hook: {human_err}"
    );
}

#[cfg(unix)]
#[test]
fn atomic_push_json_rejects_both_refs_when_update_hook_declines_one() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let seed = tmp.path().join("seed");
    git(
        Some(tmp.path()),
        &["init", "-q", "-b", "main", seed.to_str().unwrap()],
    );
    git(Some(&seed), &["config", "user.name", "Tester"]);
    git(Some(&seed), &["config", "user.email", "tester@example.com"]);
    std::fs::write(seed.join("f"), "base\n").unwrap();
    git(Some(&seed), &["add", "f"]);
    git(Some(&seed), &["commit", "-qm", "base"]);
    let bare = tmp.path().join("remote.git");
    git(
        Some(tmp.path()),
        &[
            "clone",
            "--bare",
            seed.to_str().unwrap(),
            bare.to_str().unwrap(),
        ],
    );
    write_hook(
        &bare,
        "update",
        "#!/bin/sh\ncase \"$1\" in *blocked*) echo \"policy denied $1\" >&2; exit 1;; esac\n",
    );

    let port = free_port().expect("port");
    let child = Command::new(grit_http_server_bin())
        .arg("--root")
        .arg(tmp.path())
        .arg("--bind")
        .arg(format!("127.0.0.1:{port}"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn server");
    let _guard = ChildGuard(child);
    assert!(wait_tcp(port), "grit-http-server did not listen");
    let url = format!("http://127.0.0.1:{port}/remote.git");
    git(Some(&seed), &["remote", "add", "origin", &url]);

    let out = Command::new(GRIT)
        .current_dir(&seed)
        .args([
            "push",
            "origin",
            "HEAD:refs/heads/review-ok",
            "HEAD:refs/heads/review-blocked",
            "--atomic",
            "--json",
        ])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("grit atomic push");
    assert_eq!(out.status.code(), Some(1));
    let json: Value = serde_json::from_slice(&out.stdout).expect("json stdout");
    assert_eq!(json["rejected"], true, "{json}");
    let results = json["results"].as_array().expect("per-ref results");
    assert_eq!(results.len(), 2, "{json}");
    for name in ["refs/heads/review-ok", "refs/heads/review-blocked"] {
        let result = results
            .iter()
            .find(|r| r["ref"] == name)
            .expect("ref result");
        assert_eq!(result["status"], "rejected", "{json}");
        let check = Command::new("git")
            .current_dir(&bare)
            .args(["show-ref", "--verify", name])
            .output()
            .unwrap();
        assert!(!check.status.success(), "atomic push published {name}");
    }
    git(Some(&bare), &["fsck", "--strict"]);
}
