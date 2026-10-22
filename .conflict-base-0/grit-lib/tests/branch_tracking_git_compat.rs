//! Ahead/behind counts vs `git rev-list --left-right --count` (t6040 subset).

use std::process::Command;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::refs::branch_tracking::{Standing, TipComparison};
use grit_lib::repo::Repository;
use tempfile::TempDir;

fn git_ahead_behind(root: &std::path::Path, left: &str, right: &str) -> (usize, usize) {
    let out = Command::new("git")
        .current_dir(root)
        .args([
            "rev-list",
            "--left-right",
            "--count",
            &format!("{left}...{right}"),
        ])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git rev-list");
    assert!(out.status.success());
    let s = String::from_utf8_lossy(&out.stdout);
    let mut parts = s.split('\t');
    let ahead: usize = parts.next().unwrap().trim().parse().unwrap();
    let behind: usize = parts.next().unwrap().trim().parse().unwrap();
    (ahead, behind)
}

#[test]
fn t6040_branch_tracking_ahead_behind_matches_git() {
    let dir = TempDir::new().expect("tempdir");
    Command::new("git")
        .current_dir(dir.path())
        .args(["init", "-q", "-b", "main", "."])
        .status()
        .unwrap();
    std::fs::write(dir.path().join("f"), b"1\n").unwrap();
    Command::new("git")
        .current_dir(dir.path())
        .args(["add", "f"])
        .status()
        .unwrap();
    Command::new("git")
        .current_dir(dir.path())
        .args(["commit", "-qm", "c1"])
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@e.com")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@e.com")
        .status()
        .unwrap();
    Command::new("git")
        .current_dir(dir.path())
        .args(["branch", "topic"])
        .status()
        .unwrap();
    Command::new("git")
        .current_dir(dir.path())
        .args(["checkout", "-q", "topic"])
        .status()
        .unwrap();
    std::fs::write(dir.path().join("f"), b"2\n").unwrap();
    Command::new("git")
        .current_dir(dir.path())
        .args(["commit", "-am", "c2"])
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@e.com")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@e.com")
        .status()
        .unwrap();

    let repo = Repository::discover(Some(dir.path()), &RepositoryOptions::empty()).expect("open");
    let topic = repo.refs().resolve("refs/heads/topic").expect("topic");
    let main = repo.refs().resolve("refs/heads/main").expect("main");
    let comparison = TipComparison::compute(&repo, topic, main, 1).expect("compare");
    let (git_left, git_right) = git_ahead_behind(dir.path(), "main", "topic");
    // The comparison reports how `topic` differs from `main`; git `main...topic` uses
    // left=main-only, right=topic-only commits.
    assert_eq!(comparison.ahead, git_right);
    assert_eq!(comparison.behind, git_left);
    assert_eq!(comparison.newest_ahead, vec![topic]);
    assert_eq!(comparison.standing(), Standing::Ahead);
}
