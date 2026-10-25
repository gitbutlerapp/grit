//! `grit reset --hard` and `grit switch` in a cone sparse checkout leave out-of-cone paths
//! unmaterialized with the skip-worktree bit (issue #1814).

use std::fs;
use std::path::Path;

use grit_test_support::{git, grit_cmd, unique_tmp};

fn put(dir: &Path, name: &str, contents: &str) {
    let path = dir.join(name);
    fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    fs::write(path, contents).expect("write");
}

fn sparse_repo(tag: &str) -> std::path::PathBuf {
    let dir = unique_tmp("reset-sparse-cli", tag);
    git(&dir, &["init", "-q", "-b", "main"]);
    git(&dir, &["config", "user.name", "T"]);
    git(&dir, &["config", "user.email", "t@example.com"]);
    put(&dir, "a/f", "1\n");
    put(&dir, "b/f", "1\n");
    put(&dir, "top", "1\n");
    git(&dir, &["add", "-A"]);
    git(&dir, &["commit", "-q", "-m", "c1"]);
    put(&dir, "a/f", "2\n");
    put(&dir, "b/f", "2\n");
    put(&dir, "b/g", "2\n");
    git(&dir, &["add", "-A"]);
    git(&dir, &["commit", "-q", "-m", "c2"]);
    git(&dir, &["sparse-checkout", "set", "--cone", "a"]);
    dir
}

#[test]
fn reset_hard_keeps_sparse_checkout() {
    let dir = sparse_repo("reset");
    grit_cmd(&["reset", "--hard", "HEAD~1"]).in_dir(&dir).suc();
    assert!(!dir.join("b").exists(), "b/ must stay out of the work tree");
    assert_eq!(git(&dir, &["ls-files", "-t"]), "H a/f\nS b/f\nH top\n");
    assert_eq!(git(&dir, &["status", "--porcelain"]), "");
}

#[test]
fn switch_detach_keeps_sparse_checkout() {
    let dir = sparse_repo("switch");
    grit_cmd(&["switch", "--detach", "HEAD~1"])
        .in_dir(&dir)
        .suc();
    assert!(!dir.join("b").exists(), "b/ must stay out of the work tree");
    assert_eq!(git(&dir, &["ls-files", "-t"]), "H a/f\nS b/f\nH top\n");
    assert_eq!(git(&dir, &["status", "--porcelain"]), "");
}
