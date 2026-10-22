//! Range revert apply order matches system `git revert` under committer-date skew.

use std::process::Command;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::porcelain::revert::revision_range_apply_order;
use grit_lib::repo::Repository;

fn init_repo(root: &std::path::Path) {
    Command::new("git")
        .args(["init", "-q", "-b", "main"])
        .current_dir(root)
        .status()
        .unwrap();
    Command::new("git")
        .args(["config", "user.email", "t@example.com"])
        .current_dir(root)
        .status()
        .unwrap();
    Command::new("git")
        .args(["config", "user.name", "T"])
        .current_dir(root)
        .status()
        .unwrap();
}

fn commit_with_dates(root: &std::path::Path, path: &str, body: &str, msg: &str, when: &str) {
    std::fs::write(root.join(path), body).unwrap();
    Command::new("git")
        .args(["add", path])
        .current_dir(root)
        .status()
        .unwrap();
    Command::new("git")
        .args(["commit", "-qm", msg])
        .env("GIT_AUTHOR_DATE", when)
        .env("GIT_COMMITTER_DATE", when)
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .current_dir(root)
        .status()
        .unwrap();
}

fn oid_at(root: &std::path::Path, rev: &str) -> grit_lib::objects::ObjectId {
    let out = Command::new("git")
        .args(["rev-parse", rev])
        .current_dir(root)
        .output()
        .unwrap();
    let hex = String::from_utf8_lossy(&out.stdout).trim().to_string();
    hex.parse().expect("oid")
}

#[test]
fn skewed_committer_dates_order_matches_git_revert_range() {
    let root = tempfile::tempdir().expect("tempdir");
    init_repo(root.path());
    commit_with_dates(root.path(), "f", "0\n", "base", "@1000 +0000");
    let base = oid_at(root.path(), "HEAD");
    commit_with_dates(root.path(), "f", "1\n", "c1", "@1200 +0000");
    let c1 = oid_at(root.path(), "HEAD");
    commit_with_dates(root.path(), "f", "2\n", "c2", "@1100 +0000");
    let tip = oid_at(root.path(), "HEAD");

    let repo = Repository::open(
        &root.path().join(".git"),
        &RepositoryOptions::empty().with_work_tree(root.path()),
    )
    .expect("open");
    let grit_order = revision_range_apply_order(&repo, &[tip], &[base]).expect("grit order");
    assert_eq!(grit_order, vec![tip, c1]);

    let git_wt = tempfile::tempdir().expect("git copy");
    Command::new("cp")
        .args([
            "-a",
            &format!("{}/.", root.path().display()),
            &git_wt.path().to_string_lossy(),
        ])
        .status()
        .unwrap();
    Command::new("git")
        .args(["revert", "--no-edit", &format!("{base}..{}", tip.to_hex())])
        .current_dir(git_wt.path())
        .status()
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(git_wt.path().join("f")).unwrap(),
        "0\n"
    );

    let _ = c1;
}

#[test]
fn merged_side_branch_includes_side_commit_like_git_rev_list() {
    let root = tempfile::tempdir().expect("tempdir");
    init_repo(root.path());
    std::fs::write(root.path().join("f"), "root\n").unwrap();
    Command::new("git")
        .args(["add", "f"])
        .current_dir(root.path())
        .status()
        .unwrap();
    Command::new("git")
        .args(["commit", "-qm", "root"])
        .current_dir(root.path())
        .status()
        .unwrap();
    let root_oid = oid_at(root.path(), "HEAD");

    Command::new("git")
        .args(["checkout", "-q", "-b", "side"])
        .current_dir(root.path())
        .status()
        .unwrap();
    std::fs::write(root.path().join("s"), "side\n").unwrap();
    Command::new("git")
        .args(["add", "s"])
        .current_dir(root.path())
        .status()
        .unwrap();
    Command::new("git")
        .args(["commit", "-qm", "side"])
        .current_dir(root.path())
        .status()
        .unwrap();
    let side = oid_at(root.path(), "HEAD");

    Command::new("git")
        .args(["checkout", "-q", "main"])
        .current_dir(root.path())
        .status()
        .unwrap();
    std::fs::write(root.path().join("f"), "base\n").unwrap();
    Command::new("git")
        .args(["add", "f"])
        .current_dir(root.path())
        .status()
        .unwrap();
    Command::new("git")
        .args(["commit", "-qm", "base"])
        .current_dir(root.path())
        .status()
        .unwrap();
    let base = oid_at(root.path(), "HEAD");

    Command::new("git")
        .args(["merge", "-qm", "merge side", "side"])
        .current_dir(root.path())
        .status()
        .unwrap();
    let merge = oid_at(root.path(), "HEAD");

    let git_list = Command::new("git")
        .args([
            "rev-list",
            "--topo-order",
            &format!("{base}..{}", merge.to_hex()),
        ])
        .current_dir(root.path())
        .output()
        .unwrap();
    assert!(git_list.status.success());
    let git_order: Vec<grit_lib::objects::ObjectId> = String::from_utf8_lossy(&git_list.stdout)
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| l.parse().expect("oid"))
        .collect();
    assert_eq!(git_order, vec![merge, side]);

    let repo = Repository::open(
        &root.path().join(".git"),
        &RepositoryOptions::empty().with_work_tree(root.path()),
    )
    .expect("open");
    let grit_order = revision_range_apply_order(&repo, &[merge], &[base]).expect("grit order");
    assert_eq!(grit_order, git_order);

    let _ = root_oid;
}
