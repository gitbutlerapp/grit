//! Wide-tree checkout (many modified paths) must not retain O(changed files) planning buffers.

#![cfg(target_os = "linux")]

use std::io::Write;
use std::path::Path;
use std::process::Command;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::ObjectId;
use grit_lib::porcelain::checkout::checkout_between_trees_with_progress;
use grit_lib::progress::{NeverCancel, NullProgress};
use grit_lib::repo::Repository;

const DIRS: u32 = 30;
const FILES_PER_DIR: u32 = 150;
/// Half the tree rewritten on each branch (~22.5k paths at full scale; scaled here for CI).
const DIRS_OTHER: u32 = 15;

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
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
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

fn build_wide_repo(root: &Path) -> (ObjectId, ObjectId) {
    git(root, &["init", "-q", "-b", "main"], b"");
    git(root, &["config", "user.name", "Test"], b"");
    git(root, &["config", "user.email", "t@example.com"], b"");
    for d in 0..DIRS {
        std::fs::create_dir_all(root.join(format!("d{d}"))).expect("mkdir");
        for i in 0..FILES_PER_DIR {
            std::fs::write(
                root.join(format!("d{d}/f{i}.txt")),
                format!("base {d} {i}\n").repeat(5),
            )
            .expect("write");
        }
    }
    git(root, &["add", "-A"], b"");
    git(root, &["commit", "-qm", "base"], b"");

    git(root, &["checkout", "-q", "-b", "other"], b"");
    for d in 0..DIRS_OTHER {
        for i in 0..FILES_PER_DIR {
            let path = root.join(format!("d{d}/f{i}.txt"));
            std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .expect("open")
                .write_all(b"other\n")
                .expect("append");
        }
    }
    git(root, &["commit", "-qam", "other"], b"");
    let other_tree = ObjectId::from_hex(git(root, &["rev-parse", "other^{tree}"], b"").trim())
        .expect("other tree");

    git(root, &["checkout", "-q", "main"], b"");
    for d in DIRS_OTHER..DIRS {
        for i in 0..FILES_PER_DIR {
            let path = root.join(format!("d{d}/f{i}.txt"));
            std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .expect("open")
                .write_all(b"main\n")
                .expect("append");
        }
    }
    git(root, &["commit", "-qam", "main2"], b"");
    let main_tree = ObjectId::from_hex(git(root, &["rev-parse", "main^{tree}"], b"").trim())
        .expect("main tree");
    (main_tree, other_tree)
}

#[test]
fn checkout_between_wide_trees_bounds_peak_rss() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    let (main_tree, other_tree) = build_wide_repo(root);
    git(root, &["checkout", "-q", "main"], b"");
    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");

    let changed_files = (DIRS_OTHER * FILES_PER_DIR) as i64;
    // Regression guard: pre-fix grit retained about 4 KiB of planning state per changed path.
    let max_growth_kib = changed_files * 10;

    let before = peak_rss_kib();
    checkout_between_trees_with_progress(
        &repo,
        Some(&main_tree),
        &other_tree,
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("checkout other");
    let growth = peak_rss_kib() - before;
    eprintln!("wide-tree checkout peak RSS growth: {growth} KiB ({changed_files} paths)");
    assert!(
        growth < max_growth_kib,
        "checkout retained too much for {changed_files} paths: peak RSS grew {growth} KiB (limit {max_growth_kib} KiB)"
    );

    assert_eq!(git(root, &["write-tree"], b"").trim(), other_tree.to_hex());
    git(root, &["fsck", "--strict", "--no-progress"], b"");
}
