//! Ahead/behind counts vs `git rev-list --left-right --count` (t6040 subset).

use std::process::Command;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::refs::branch_tracking::{stat_branch_pair, AheadBehindMode, TrackingStat};
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
    let grit_stat =
        stat_branch_pair(&repo, "topic", "refs/heads/main", AheadBehindMode::Full).expect("stat");
    let (git_left, git_right) = git_ahead_behind(dir.path(), "main", "topic");
    match grit_stat {
        TrackingStat::Diverged { ahead, behind, .. } => {
            // `stat_branch_pair` reports how `topic` differs from `main`; git `main...topic` uses
            // left=main-only, right=topic-only commits.
            assert_eq!(ahead, git_right);
            assert_eq!(behind, git_left);
        }
        other => panic!("expected diverged stat, got {other:?}"),
    }
}
