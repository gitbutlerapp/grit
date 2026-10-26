//! Checkout batches must bound buffered contents as well as the number of paths.

#![cfg(target_os = "linux")]

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::ObjectId;
use grit_lib::porcelain::checkout::checkout_between_trees;
use grit_lib::porcelain::switch::{switch_repository, SwitchMode, SwitchOptions};
use grit_lib::progress::{NeverCancel, ProgressSink};
use grit_lib::repo::Repository;

fn git(root: &Path, args: &[&str], input: &[u8]) -> String {
    let mut child = Command::new("git")
        .args(args)
        .current_dir(root)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn git");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(input)
        .expect("write stdin");
    let out = child.wait_with_output().expect("wait for git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).expect("UTF-8 output")
}

struct SlowProgress;

impl ProgressSink for SlowProgress {
    fn inc(&mut self, _units: u64) {
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

fn peak_rss_kib() -> i64 {
    std::fs::read_to_string("/proc/self/status")
        .expect("process status")
        .lines()
        .find_map(|line| line.strip_prefix("VmHWM:"))
        .expect("VmHWM")
        .split_whitespace()
        .next()
        .expect("RSS value")
        .parse()
        .expect("RSS KiB")
}

#[test]
fn checkout_bounds_medium_blob_buffers_for_serial_and_parallel_preparation() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    git(root, &["init", "-q", "-b", "main"], b"");
    git(root, &["config", "checkout.workers", "4"], b"");
    git(root, &["config", "user.name", "Test"], b"");
    git(root, &["config", "user.email", "t@example.com"], b"");
    let mut contents = vec![b'x'; 512 * 1024];
    let tree_input: String = (0..256u32)
        .map(|i| {
            contents[..4].copy_from_slice(&i.to_le_bytes());
            let blob = git(root, &["hash-object", "-w", "--stdin"], &contents);
            format!("100644 blob {}\tf{i:04}.txt\n", blob.trim())
        })
        .collect();
    let tree = git(root, &["mktree"], tree_input.as_bytes());
    let commit = git(root, &["commit-tree", tree.trim(), "-m", "many files"], b"");
    git(root, &["update-ref", "refs/heads/main", commit.trim()], b"");
    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let tree = ObjectId::from_hex(tree.trim()).expect("tree oid");

    let before = peak_rss_kib();
    checkout_between_trees(&repo, None, &tree).expect("checkout");
    let growth = peak_rss_kib() - before;
    eprintln!("checkout peak RSS growth: {growth} KiB, initial {before} KiB");
    assert!(
        growth < 16 * 1024,
        "checkout buffered too much: peak RSS grew {growth} KiB"
    );
    assert_eq!(git(root, &["write-tree"], b"").trim(), tree.to_hex());
    assert!(git(root, &["status", "--porcelain"], b"").trim().is_empty());
    git(root, &["fsck", "--strict", "--no-progress"], b"");

    let empty_tree = git(root, &["mktree"], b"");
    let empty = git(
        root,
        &[
            "commit-tree",
            empty_tree.trim(),
            "-p",
            commit.trim(),
            "-m",
            "empty",
        ],
        b"",
    );
    git(root, &["update-ref", "refs/heads/empty", empty.trim()], b"");
    git(root, &["checkout", "-q", "empty"], b"");
    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("reopen");
    let before = peak_rss_kib();
    switch_repository(
        &repo,
        &SwitchOptions {
            mode: SwitchMode::Existing {
                branch: "main".into(),
                oid: ObjectId::from_hex(commit.trim()).expect("commit oid"),
            },
            dry_run: false,
            hooks: grit_lib::PostOperationHooks::Run,
        },
        &mut SlowProgress,
        &NeverCancel,
    )
    .expect("switch with parallel preparation");
    let growth = peak_rss_kib() - before;
    eprintln!("parallel checkout peak RSS growth: {growth} KiB, initial {before} KiB");
    assert!(
        growth < 16 * 1024,
        "parallel preparation buffered too much: peak RSS grew {growth} KiB"
    );
    assert_eq!(git(root, &["write-tree"], b"").trim(), tree.to_hex());
    assert!(git(root, &["status", "--porcelain"], b"").trim().is_empty());
    git(root, &["fsck", "--strict", "--no-progress"], b"");

    git(root, &["checkout", "-q", "empty"], b"");
    let large = git(
        root,
        &["hash-object", "-w", "--stdin"],
        &vec![b'x'; 64 * 1024 * 1024],
    );
    let tree_input = format!("100644 blob {}\tlarge.dat\n", large.trim());
    let large_tree = git(root, &["mktree"], tree_input.as_bytes());
    let large_commit = git(
        root,
        &["commit-tree", large_tree.trim(), "-m", "cached large blob"],
        b"",
    );
    git(
        root,
        &["update-ref", "refs/heads/cached-large", large_commit.trim()],
        b"",
    );
    git(
        root,
        &["symbolic-ref", "HEAD", "refs/heads/cached-large"],
        b"",
    );
    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("reopen");
    let large = ObjectId::from_hex(large.trim()).expect("large blob oid");
    // Retain the result as well as the ODB's cached copy, so another whole-object copy
    // would raise the process high-water mark during the following checkout.
    let retained = repo.odb().read(&large).expect("cache large blob");
    let large_tree = ObjectId::from_hex(large_tree.trim()).expect("large tree oid");
    let before = peak_rss_kib();
    checkout_between_trees(&repo, None, &large_tree).expect("checkout cached large blob");
    let growth = peak_rss_kib() - before;
    eprintln!("cached large-blob checkout peak RSS growth: {growth} KiB");
    assert!(
        growth < 16 * 1024,
        "checkout copied a cached large blob: peak RSS grew {growth} KiB"
    );
    assert_eq!(retained.data.len(), 64 * 1024 * 1024);
    assert_eq!(git(root, &["write-tree"], b"").trim(), large_tree.to_hex());
    assert!(git(root, &["status", "--porcelain"], b"").trim().is_empty());
    git(root, &["fsck", "--strict", "--no-progress"], b"");
}
