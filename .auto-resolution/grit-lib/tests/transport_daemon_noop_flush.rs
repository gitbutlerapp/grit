//! Regression for issue #998: streaming clients must send a flush pkt-line (`0000`)
//! after the ref advertisement / ls-refs round when they have no wants or push
//! commands, or `git daemon` logs "hung up unexpectedly".

#![cfg(unix)]

use std::io::Read;
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::remote::transfer::{FetchOptions, PushOptions, PushRefSpec, TagMode};
use grit_lib::remote::Remote;
use grit_lib::repo::Repository;

fn git(dir: Option<&Path>, args: &[&str]) -> String {
    let mut cmd = Command::new("git");
    if let Some(d) = dir {
        cmd.current_dir(d);
    }
    let out = cmd
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

fn build_source(dir: &Path) {
    git(Some(dir), &["init", "-q", "-b", "main", "."]);
    std::fs::write(dir.join("a.txt"), "one\n").unwrap();
    git(Some(dir), &["add", "a.txt"]);
    git(Some(dir), &["commit", "-q", "-m", "c1"]);
}

fn free_port() -> Option<u16> {
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

fn wait_tcp(port: u16) -> bool {
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

struct ChildGuard(Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct DaemonLogFixture {
    _tmp: tempfile::TempDir,
    _guard: ChildGuard,
    url: String,
    log_path: PathBuf,
}

fn daemon_log_fixture() -> Option<DaemonLogFixture> {
    let tmp = tempfile::tempdir().ok()?;
    let work = tmp.path().join("work");
    std::fs::create_dir_all(&work).ok()?;
    build_source(&work);
    let base = tmp.path().join("srv");
    std::fs::create_dir_all(&base).ok()?;
    let bare = base.join("r.git");
    git(Some(&work), &["clone", "-q", "--bare", ".", bare.to_str()?]);
    let log_path = tmp.path().join("daemon.log");
    let port = free_port()?;
    let log_file = std::fs::File::create(&log_path).ok()?;
    let child = Command::new("git")
        .arg("daemon")
        .arg("--listen=127.0.0.1")
        .arg(format!("--port={port}"))
        .arg("--reuseaddr")
        .arg("--export-all")
        .arg("--enable=receive-pack")
        .arg(format!("--base-path={}", base.display()))
        .arg(&base)
        .stdout(Stdio::null())
        .stderr(log_file)
        .spawn()
        .ok()?;
    if !wait_tcp(port) {
        return None;
    }
    Some(DaemonLogFixture {
        url: format!("git://127.0.0.1:{port}/r.git"),
        log_path,
        _tmp: tmp,
        _guard: ChildGuard(child),
    })
}

fn daemon_log_contains_hung_up(log_path: &Path) -> bool {
    let mut s = String::new();
    std::fs::File::open(log_path)
        .and_then(|mut f| f.read_to_string(&mut s))
        .ok();
    s.contains("hung up unexpectedly")
}

#[test]
fn git_daemon_noop_fetch_and_push_send_flush_not_hangup() {
    let Some(fixture) = daemon_log_fixture() else {
        return;
    };

    let local = fixture._tmp.path().join("local");
    std::fs::create_dir_all(&local).unwrap();
    git(
        Some(&local.parent().unwrap()),
        &[
            "clone",
            "-q",
            &fixture.url,
            local.file_name().unwrap().to_str().unwrap(),
        ],
    );
    // `git clone` may leave benign daemon noise; measure only grit sessions.
    let _ = std::fs::write(&fixture.log_path, "");
    let repo = Repository::open(
        &local.join(".git"),
        &RepositoryOptions::empty().with_work_tree(&local),
    )
    .unwrap();
    let remote = Remote::from_url(&fixture.url).unwrap();

    let fetch_opts = FetchOptions {
        refspecs: vec!["+refs/heads/*:refs/remotes/origin/*".to_owned()],
        tags: TagMode::Following,
        ..Default::default()
    };

    let first = remote
        .fetch(
            &repo,
            fetch_opts.clone(),
            &mut grit_lib::remote::fetch::NoProgress,
            None,
        )
        .unwrap();
    assert_eq!(first.transfer.bytes, 0, "expected noop fetch after clone");

    remote
        .fetch(
            &repo,
            fetch_opts,
            &mut grit_lib::remote::fetch::NoProgress,
            None,
        )
        .unwrap();

    let main_tip = grit_lib::refs::resolve_ref(repo.git_dir(), "refs/heads/main").unwrap();
    remote
        .push(
            &repo,
            &[PushRefSpec {
                src: Some(main_tip),
                src_ref: None,
                dst: "refs/heads/main".to_owned(),
                force: false,
                delete: false,
                expected_old: None,
                expect_absent: false,
                ..Default::default()
            }],
            PushOptions::default(),
            &mut grit_lib::remote::fetch::NoProgress,
            None,
        )
        .unwrap();

    assert!(
        !daemon_log_contains_hung_up(&fixture.log_path),
        "git daemon must not log hung up unexpectedly; log:\n{}",
        std::fs::read_to_string(&fixture.log_path).unwrap_or_default()
    );
}
