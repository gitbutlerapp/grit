//! Revert commit selection and range apply order match system `git` under committer-date skew.

use std::process::Command;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::porcelain::revert::{commits_to_revert, revision_range_apply_order};
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

fn git(root: &std::path::Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(root)
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?}");
}

fn git_rev_list_topo(root: &std::path::Path, specs: &[&str]) -> Vec<grit_lib::objects::ObjectId> {
    let out = Command::new("git")
        .arg("rev-list")
        .arg("--topo-order")
        .args(specs)
        .current_dir(root)
        .output()
        .unwrap();
    assert!(out.status.success(), "git rev-list {specs:?}");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| l.parse().expect("oid"))
        .collect()
}

fn open(root: &std::path::Path) -> Repository {
    Repository::open(
        &root.join(".git"),
        &RepositoryOptions::empty().with_work_tree(root),
    )
    .expect("open")
}

fn specs(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| (*s).to_owned()).collect()
}

/// `base` - c1 - c2 - c3 on `main` with skewed committer dates, plus `side` forked at `base`.
fn linear_with_side(root: &std::path::Path) -> [grit_lib::objects::ObjectId; 4] {
    init_repo(root);
    commit_with_dates(root, "f", "0\n", "base", "@1000 +0000");
    let base = oid_at(root, "HEAD");
    git(root, &["branch", "side"]);
    commit_with_dates(root, "f", "1\n", "c1", "@1300 +0000");
    let c1 = oid_at(root, "HEAD");
    commit_with_dates(root, "f", "2\n", "c2", "@1100 +0000");
    let c2 = oid_at(root, "HEAD");
    commit_with_dates(root, "f", "3\n", "c3", "@1200 +0000");
    let c3 = oid_at(root, "HEAD");
    git(root, &["checkout", "-q", "side"]);
    commit_with_dates(root, "s", "s\n", "s1", "@1250 +0000");
    git(root, &["checkout", "-q", "main"]);
    [base, c1, c2, c3]
}

#[test]
fn commits_to_revert_range_spellings_match_git_rev_list() {
    let root = tempfile::tempdir().expect("tempdir");
    let [base, ..] = linear_with_side(root.path());
    let repo = open(root.path());
    let base_hex = base.to_hex();
    let range = format!("{base_hex}..HEAD");
    let exclude = format!("^{base_hex}");
    let cases: [&[&str]; 5] = [
        &[&range],
        &[&exclude, "HEAD"],
        &["HEAD", &exclude],
        &["HEAD~2..HEAD~1", "HEAD"],
        &["main...side"],
    ];
    for case in cases {
        let grit = commits_to_revert(&repo, &specs(case)).expect("commits_to_revert");
        assert_eq!(grit, git_rev_list_topo(root.path(), case), "specs {case:?}");
    }
}

#[test]
fn commits_to_revert_plain_specs_keep_argument_order() {
    let root = tempfile::tempdir().expect("tempdir");
    let [_, c1, c2, c3] = linear_with_side(root.path());
    let repo = open(root.path());
    let got = commits_to_revert(&repo, &specs(&[&c1.to_hex(), "HEAD", "HEAD~1"]))
        .expect("commits_to_revert");
    assert_eq!(got, vec![c1, c3, c2]);
    let single = commits_to_revert(&repo, &specs(&["main"])).expect("single");
    assert_eq!(single, vec![c3]);
}

#[test]
fn commits_to_revert_rejects_non_commit_spec() {
    let root = tempfile::tempdir().expect("tempdir");
    linear_with_side(root.path());
    let repo = open(root.path());
    assert!(commits_to_revert(&repo, &specs(&["HEAD", "HEAD^{tree}"])).is_err());
    assert!(commits_to_revert(&repo, &specs(&["no-such-branch"])).is_err());
}
