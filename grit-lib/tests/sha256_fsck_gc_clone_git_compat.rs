//! Regression for issue #1962: SHA-256 repos from git must fsck/gc/clone cleanly.

use grit_lib::progress::CancelToken;
use grit_lib::progress::NullProgress;
use grit_test_support::{free_port, git};
use std::net::TcpStream;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::maintenance::repository_fsck::FsckOptions;
use grit_lib::maintenance::GcOptions;
use grit_lib::objects::HashAlgo;
use grit_lib::remote::clone::{clone, CloneOptions};
use grit_lib::repo::Repository;
use grit_lib::test_support::refs::all_reflog_oids;
use grit_test_support::git_supports_sha256;
use tempfile::TempDir;

fn git_init_sha256(dir: &Path) {
    git(
        dir,
        &["init", "-q", "-b", "main", "--object-format=sha256", "."],
    );
}

#[test]
fn git_sha256_repo_reflog_null_oid_excluded_from_fsck_and_gc() {
    if !git_supports_sha256() {
        return;
    }
    let td = TempDir::new().expect("tempdir");
    git_init_sha256(td.path());
    std::fs::write(td.path().join("a"), "hi\n").unwrap();
    git(td.path(), &["add", "a"]);
    git(td.path(), &["commit", "-qm", "one"]);

    let repo = Repository::discover(Some(td.path()), &RepositoryOptions::empty()).expect("open");
    let null_hex = "0".repeat(HashAlgo::Sha256.hex_len());
    let oids = all_reflog_oids(repo.git_dir()).expect("reflog oids");
    assert!(
        !oids.iter().any(|o| o.to_hex() == null_hex),
        "64-zero null oid must not appear in reflog reachability roots"
    );

    let report = repo
        .maintenance()
        .fsck(
            &FsckOptions::default(),
            &mut NullProgress,
            &CancelToken::never(),
        )
        .expect("fsck");
    assert!(
        report.issues.is_empty(),
        "fsck should be clean: {:?}",
        report.issues
    );

    repo.maintenance()
        .gc(
            &GcOptions::default(),
            &mut NullProgress,
            &CancelToken::never(),
        )
        .expect("gc");
}

#[test]
fn grit_clone_local_git_sha256_repo() {
    if !git_supports_sha256() {
        return;
    }
    let parent = TempDir::new().expect("tempdir");
    let source = parent.path().join("source");
    std::fs::create_dir(&source).unwrap();
    git_init_sha256(&source);
    std::fs::write(source.join("a"), "hi\n").unwrap();
    git(&source, &["add", "a"]);
    git(&source, &["commit", "-qm", "one"]);

    let dest = parent.path().join("grit-clone");
    let outcome = clone(
        CloneOptions::new(source.to_string_lossy().into_owned(), dest.clone())
            .with_url(source.to_string_lossy().into_owned())
            .with_dest(dest.clone()),
        &mut NoProgress,
        &CancelToken::never(),
        None,
    )
    .expect("clone");
    assert_eq!(outcome.repo.odb().hash_algo(), HashAlgo::Sha256);

    let expected = Command::new("git")
        .current_dir(&source)
        .args(["rev-parse", "HEAD"])
        .output()
        .expect("git rev-parse");
    assert!(expected.status.success());
    let expected = String::from_utf8_lossy(&expected.stdout).trim().to_owned();

    let cloned = Command::new("git")
        .current_dir(&dest)
        .args(["rev-parse", "HEAD"])
        .output()
        .expect("cloned rev-parse");
    assert!(cloned.status.success());
    assert_eq!(String::from_utf8_lossy(&cloned.stdout).trim(), expected);

    git(&dest, &["fsck", "--strict"]);
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

fn spawn_git_daemon(base_path: &Path, port: u16) -> Option<Child> {
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

struct DaemonGuard(Child);
impl Drop for DaemonGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Clone over git:// into a normal SHA-1-initialized destination (wire path), then checkout.
#[test]
fn grit_clone_git_daemon_sha256_repo_after_sha1_init() {
    if !git_supports_sha256() {
        return;
    }
    let tmp = TempDir::new().expect("tempdir");
    let work = tmp.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    git_init_sha256(&work);
    std::fs::write(work.join("a.txt"), "payload\n").unwrap();
    git(&work, &["add", "a.txt"]);
    git(&work, &["commit", "-qm", "one"]);

    let base = tmp.path().join("srv");
    std::fs::create_dir_all(&base).unwrap();
    let bare = base.join("repo.git");
    git(
        &work,
        &["clone", "-q", "--bare", ".", bare.to_str().expect("utf8")],
    );
    git(&bare, &["symbolic-ref", "HEAD", "refs/heads/main"]);
    assert_eq!(
        String::from_utf8_lossy(
            &Command::new("git")
                .current_dir(&bare)
                .args(["rev-parse", "--show-object-format"])
                .output()
                .expect("format")
                .stdout
        )
        .trim(),
        "sha256"
    );

    let Some(port) = free_port() else {
        eprintln!("SKIP: no free port");
        return;
    };
    let Some(child) = spawn_git_daemon(&base, port) else {
        eprintln!("SKIP: git daemon unavailable");
        return;
    };
    let _guard = DaemonGuard(child);
    if !wait_ready(port) {
        eprintln!("SKIP: daemon not ready");
        return;
    }

    let url = format!("git://127.0.0.1:{port}/repo.git");
    let dest = tmp.path().join("clone");
    let outcome = clone(
        CloneOptions::new(url, dest.clone()),
        &mut NoProgress,
        &CancelToken::never(),
        None,
    )
    .expect("git:// clone of sha256 repo");
    assert_eq!(outcome.repo.odb().hash_algo(), HashAlgo::Sha256);
    assert!(
        dest.join("a.txt").is_file(),
        "checkout must materialize tree"
    );
    assert_eq!(
        std::fs::read_to_string(dest.join("a.txt")).expect("read"),
        "payload\n"
    );
    git(&dest, &["fsck", "--strict"]);
}
