//! End-to-end tests for the `gritx-fetch` / `gritx-push` examples: they discover
//! the default remote, report the transport + auth, and run the operation. These
//! use real fixtures (an on-disk remote and, when available, a `git daemon`) and
//! cross-check the result with the system `git`.

use std::net::TcpListener;
use std::net::TcpStream;
use std::path::Path;
use std::process::Child;
use std::process::Command;
use std::process::Output;
use std::process::Stdio;
use std::time::Duration;
use std::time::Instant;

use anyhow::bail;
use anyhow::Context as _;
use anyhow::Result;

fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .output()
        .context("run git")?;
    if !out.status.success() {
        bail!(
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

fn fetch(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_gritx-fetch"))
        .args(args)
        .current_dir(dir)
        .output()
        .expect("run gritx-fetch")
}

fn push(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_gritx-push"))
        .args(args)
        .current_dir(dir)
        .output()
        .expect("run gritx-push")
}

/// Build a bare source repo with one commit on `main` plus a `topic` branch, and
/// a `consumer` repo whose `origin` points at it. Returns (consumer, bare).
fn source_and_consumer(root: &Path, origin_url: &str) -> Result<(std::path::PathBuf, String)> {
    let work = root.join("work");
    std::fs::create_dir_all(&work)?;
    git(&work, &["init", "-q", "-b", "main", "."])?;
    git(&work, &["commit", "-q", "--allow-empty", "-m", "c1"])?;
    git(&work, &["branch", "topic"])?;
    let bare = root.join("repo.git");
    git(
        root,
        &[
            "clone",
            "-q",
            "--bare",
            work.to_str().unwrap(),
            bare.to_str().unwrap(),
        ],
    )?;
    let main_oid = git(&bare, &["rev-parse", "refs/heads/main"])?;

    let consumer = root.join("consumer");
    std::fs::create_dir_all(&consumer)?;
    git(&consumer, &["init", "-q", "-b", "main", "."])?;
    let url = if origin_url.is_empty() {
        bare.to_str().unwrap().to_owned()
    } else {
        origin_url.to_owned()
    };
    git(&consumer, &["remote", "add", "origin", &url])?;
    Ok((consumer, main_oid))
}

#[test]
fn fetch_and_push_over_local_remote() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let (consumer, main_oid) = source_and_consumer(tmp.path(), "")?;
    let bare = tmp.path().join("repo.git");

    // --- fetch: discovers the local transport + "no auth", lands tracking refs.
    let out = fetch(&consumer, &["origin"]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "gritx-fetch failed: {err}");
    assert!(err.contains("transport: local (file)"), "stderr: {err}");
    assert!(err.contains("auth:"), "stderr: {err}");
    assert!(err.contains("none (local repository)"), "stderr: {err}");
    assert_eq!(
        git(&consumer, &["rev-parse", "refs/remotes/origin/main"])?,
        main_oid,
        "origin/main should match the source after fetch"
    );
    assert!(git(&consumer, &["rev-parse", "refs/remotes/origin/topic"]).is_ok());

    // --- push: a new local commit on main updates the bare remote.
    git(
        &consumer,
        &["reset", "-q", "--hard", "refs/remotes/origin/main"],
    )?;
    git(&consumer, &["commit", "-q", "--allow-empty", "-m", "c2"])?;
    let head = git(&consumer, &["rev-parse", "HEAD"])?;

    let out = push(&consumer, &["origin"]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "gritx-push failed: {err}");
    assert!(err.contains("transport: local (file)"), "stderr: {err}");
    assert_eq!(
        git(&bare, &["rev-parse", "refs/heads/main"])?,
        head,
        "remote main should match the pushed HEAD"
    );

    // --- a non-fast-forward push is rejected; --force succeeds.
    let other = tmp.path().join("other");
    git(
        tmp.path(),
        &[
            "clone",
            "-q",
            bare.to_str().unwrap(),
            other.to_str().unwrap(),
        ],
    )?;
    git(
        &other,
        &["commit", "-q", "--allow-empty", "-m", "remote-adv"],
    )?;
    git(&other, &["push", "-q", "origin", "main"])?;
    git(
        &consumer,
        &["commit", "-q", "--allow-empty", "-m", "local-div"],
    )?;

    let out = push(&consumer, &["origin"]);
    assert!(
        !out.status.success(),
        "non-ff push should fail: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("non-fast-forward"),
        "stdout: {}",
        String::from_utf8_lossy(&out.stdout)
    );

    let forced = git(&consumer, &["rev-parse", "HEAD"])?;
    let out = push(&consumer, &["--force", "origin"]);
    assert!(
        out.status.success(),
        "forced push should succeed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(git(&bare, &["rev-parse", "refs/heads/main"])?, forced);

    Ok(())
}

#[test]
fn fetch_over_git_daemon_reports_anonymous_auth() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    // The daemon serves <base>/repo.git over git://.
    let base = tmp.path().join("daemon");
    std::fs::create_dir_all(&base)?;
    let work = tmp.path().join("work");
    std::fs::create_dir_all(&work)?;
    git(&work, &["init", "-q", "-b", "main", "."])?;
    git(&work, &["commit", "-q", "--allow-empty", "-m", "c1"])?;
    let served = base.join("repo.git");
    git(
        tmp.path(),
        &[
            "clone",
            "-q",
            "--bare",
            work.to_str().unwrap(),
            served.to_str().unwrap(),
        ],
    )?;
    let main_oid = git(&served, &["rev-parse", "refs/heads/main"])?;

    let Some(port) = free_port() else {
        eprintln!("SKIP: no free port");
        return Ok(());
    };
    let daemon = Command::new("git")
        .args([
            "daemon",
            "--listen=127.0.0.1",
            &format!("--port={port}"),
            "--reuseaddr",
            "--export-all",
            &format!("--base-path={}", base.display()),
        ])
        .arg(&base)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    let Ok(daemon) = daemon else {
        eprintln!("SKIP: `git daemon` unavailable");
        return Ok(());
    };
    let _guard = DaemonGuard(daemon);
    if !wait_ready(port) {
        eprintln!("SKIP: git daemon did not become ready");
        return Ok(());
    }

    let consumer = tmp.path().join("consumer");
    std::fs::create_dir_all(&consumer)?;
    git(&consumer, &["init", "-q", "-b", "main", "."])?;
    let url = format!("git://127.0.0.1:{port}/repo.git");
    git(&consumer, &["remote", "add", "origin", &url])?;

    let out = fetch(&consumer, &["origin"]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "gritx-fetch failed: {err}");
    assert!(
        err.contains("transport: git:// (anonymous daemon)"),
        "stderr: {err}"
    );
    assert!(
        err.contains("none (anonymous git:// protocol)"),
        "stderr: {err}"
    );
    assert_eq!(
        git(&consumer, &["rev-parse", "refs/remotes/origin/main"])?,
        main_oid
    );
    Ok(())
}

struct DaemonGuard(Child);
impl Drop for DaemonGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn free_port() -> Option<u16> {
    let l = TcpListener::bind(("127.0.0.1", 0)).ok()?;
    let p = l.local_addr().ok()?.port();
    Some(p)
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
