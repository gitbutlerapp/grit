//! Compare `diff_commits` file sets with `git diff --raw` for the same OIDs.

use std::path::Path;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::diff::diff_commits;
use grit_lib::objects::ObjectId;
use grit_lib::repo::Repository;
use grit_test_support::git;
use tempfile::TempDir;

fn init_repo(dir: &Path) {
    git(dir, &["init", "-q", "-b", "main", "."]);
    git(dir, &["config", "user.email", "u@example.com"]);
    git(dir, &["config", "user.name", "U"]);
}

fn commit_all(dir: &Path, msg: &str) -> ObjectId {
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-qm", msg]);
    git(dir, &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .expect("oid")
}

fn raw_paths(dir: &Path, base: &str, tip: &str) -> Vec<String> {
    git(dir, &["diff", "--raw", base, tip])
        .lines()
        .filter_map(|line| {
            let parts: Vec<_> = line.split_whitespace().collect();
            parts.last().map(|p| (*p).to_owned())
        })
        .collect()
}

#[test]
fn diff_commits_matches_git_raw_file_set() {
    let dir = TempDir::new().expect("tempdir");
    init_repo(dir.path());
    std::fs::write(dir.path().join("a.txt"), "one\n").expect("write");
    std::fs::write(dir.path().join("b.txt"), "two\n").expect("write");
    let c1 = commit_all(dir.path(), "first");
    std::fs::write(dir.path().join("a.txt"), "one changed\n").expect("write");
    std::fs::write(dir.path().join("c.txt"), "new\n").expect("write");
    let c2 = commit_all(dir.path(), "second");

    let root = dir.path();
    let repo = Repository::open(
        &root.join(".git"),
        &RepositoryOptions::empty().with_work_tree(root),
    )
    .expect("open");
    let entries = diff_commits(repo.odb(), &c1, &c2).expect("diff_commits");
    let mut grit_paths: Vec<_> = entries.iter().map(|e| e.path().to_owned()).collect();
    grit_paths.sort();

    let mut git_paths = raw_paths(dir.path(), &c1.to_hex(), &c2.to_hex());
    git_paths.sort();
    assert_eq!(grit_paths, git_paths, "file sets must match git diff --raw");
}

#[test]
fn diff_commits_two_commit_endpoints_match_git() {
    let dir = TempDir::new().expect("tempdir");
    init_repo(dir.path());
    std::fs::write(dir.path().join("x"), "a\n").expect("write");
    commit_all(dir.path(), "1");
    std::fs::write(dir.path().join("x"), "b\n").expect("write");
    let tip = commit_all(dir.path(), "2");
    let base: ObjectId = git(dir.path(), &["rev-parse", "HEAD~1"])
        .trim()
        .parse()
        .expect("oid");

    let root = dir.path();
    let repo = Repository::open(
        &root.join(".git"),
        &RepositoryOptions::empty().with_work_tree(root),
    )
    .expect("open");
    let entries = diff_commits(repo.odb(), &base, &tip).expect("diff");
    assert!(
        entries.iter().any(|e| e.path() == "x"),
        "expected change to x"
    );
}
