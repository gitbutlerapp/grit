//! Successful push, fetch and clone output must never echo URL credentials
//! (issue #2151), in human, `--json` and `--markdown` modes, while config keeps
//! the URL as given.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::net::TcpListener;
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const GRIT: &str = env!("CARGO_BIN_EXE_grit");

struct ChildGuard(std::process::Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
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

fn commit(dir: &Path, content: &str) {
    std::fs::write(dir.join("a"), content).unwrap();
    git(dir, &["add", "a"]);
    git(dir, &["commit", "-qm", content]);
}

/// Runs `grit` in `dir`, requires success, and returns stdout followed by stderr.
fn grit_ok(dir: &Path, args: &[&str]) -> String {
    let out = Command::new(GRIT)
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("grit");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "grit {args:?} failed: {text}");
    text
}

fn assert_no_credentials(args: &[&str], output: &str, host: &str, expect: &[&str]) {
    assert!(!output.contains("s3cret"), "grit {args:?} leaked: {output}");
    assert!(!output.contains("alice"), "grit {args:?} leaked: {output}");
    assert!(output.contains(host), "grit {args:?} lost host: {output}");
    for needle in expect {
        assert!(
            output.contains(needle),
            "grit {args:?} missing {needle:?}: {output}"
        );
    }
}

fn spawn_auth_server(root: &Path) -> (ChildGuard, u16) {
    let port = TcpListener::bind("127.0.0.1:0")
        .and_then(|l| l.local_addr())
        .expect("free port")
        .port();
    let child = Command::new(grit_test_support::grit_http_server_binary())
        .arg("--root")
        .arg(root)
        .arg("--bind")
        .arg(format!("127.0.0.1:{port}"))
        .args(["--require-auth", "alice:s3cret"])
        .env("GRIT_BIN", GRIT)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn grit-http-server");
    let guard = ChildGuard(child);
    let deadline = Instant::now() + Duration::from_secs(30);
    while std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
        assert!(Instant::now() < deadline, "grit-http-server did not listen");
        thread::sleep(Duration::from_millis(50));
    }
    (guard, port)
}

#[test]
fn push_fetch_and_clone_success_output_redacts_url_credentials() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().join("root");
    std::fs::create_dir_all(&root).unwrap();
    git(&root, &["init", "-q", "--bare", "-b", "main", "auth.git"]);
    let (_server, port) = spawn_auth_server(&root);
    let host = format!("http://127.0.0.1:{port}/auth.git");
    let url = format!("http://alice:s3cret@127.0.0.1:{port}/auth.git");

    let work = tmp.path().join("w");
    std::fs::create_dir_all(&work).unwrap();
    git(&work, &["init", "-q", "-b", "main"]);
    git(&work, &["config", "user.name", "A"]);
    git(&work, &["config", "user.email", "a@example.com"]);

    let pushed = [host.as_str(), "refs/heads/main"];
    for mode in [None, Some("--json"), Some("--markdown")] {
        commit(&work, &format!("{mode:?}"));
        for dry_run in [true, false] {
            let mut args: Vec<&str> = mode.into_iter().collect();
            args.push("push");
            if dry_run {
                args.push("--dry-run");
            }
            args.extend([url.as_str(), "main"]);
            let out = grit_ok(&work, &args);
            assert_no_credentials(&args, &out, &host, &pushed);
        }
    }

    for mode in [None, Some("--json"), Some("--markdown")] {
        let mut args: Vec<&str> = mode.into_iter().collect();
        args.extend(["fetch", url.as_str()]);
        let out = grit_ok(&work, &args);
        assert_no_credentials(&args, &out, &host, &[]);

        let dest = tmp
            .path()
            .join(format!("clone-{}", mode.unwrap_or("human")));
        let mut args: Vec<&str> = mode.into_iter().collect();
        args.extend(["clone", url.as_str(), dest.to_str().unwrap()]);
        let out = grit_ok(tmp.path(), &args);
        if mode == Some("--json") {
            assert_no_credentials(&args, &out, &host, &[]);
        } else {
            assert!(!out.contains("s3cret"), "grit {args:?} leaked: {out}");
            assert!(!out.contains("alice"), "grit {args:?} leaked: {out}");
        }
        git(&dest, &["fsck", "--no-progress"]);
        let stored = Command::new("git")
            .args(["config", "remote.origin.url"])
            .current_dir(&dest)
            .output()
            .expect("git config");
        assert_eq!(String::from_utf8_lossy(&stored.stdout).trim(), url);
    }
}
