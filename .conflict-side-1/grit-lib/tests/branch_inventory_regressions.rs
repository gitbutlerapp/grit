//! Regressions for branch inventory review findings.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::process::Command;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::error::Error;
use grit_lib::refs::branch_tracking::{list_branches, BranchDetail, BranchListQuery, BranchScope};
use grit_lib::refs::listing::{RefQuery, RefSort, SortKey};
use grit_lib::repo::Repository;
use tempfile::TempDir;

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@e.com")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@e.com")
        .status()
        .expect("git");
    assert!(status.success(), "git {args:?}");
}

fn open(dir: &Path) -> Repository {
    Repository::discover(Some(dir), &RepositoryOptions::empty()).expect("open")
}

#[test]
fn upstream_uses_custom_fetch_refspec_mapping() {
    let dir = TempDir::new().expect("tempdir");
    git(dir.path(), &["init", "-q", "-b", "main"]);
    std::fs::write(dir.path().join("f"), b"x").unwrap();
    git(dir.path(), &["add", "f"]);
    git(dir.path(), &["commit", "-qm", "c1"]);
    git(
        dir.path(),
        &["config", "remote.origin.url", "/tmp/upstream-placeholder"],
    );
    git(
        dir.path(),
        &[
            "config",
            "--add",
            "remote.origin.fetch",
            "+refs/heads/*:refs/custom/origin/*",
        ],
    );
    git(
        dir.path(),
        &["update-ref", "refs/custom/origin/main", "HEAD"],
    );
    git(dir.path(), &["branch", "-q", "topic"]);
    git(dir.path(), &["config", "branch.topic.remote", "origin"]);
    git(
        dir.path(),
        &["config", "branch.topic.merge", "refs/heads/main"],
    );

    let git_upstream = Command::new("git")
        .current_dir(dir.path())
        .args(["for-each-ref", "--format=%(upstream)", "refs/heads/topic"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git");
    assert!(git_upstream.status.success());
    let expected = String::from_utf8_lossy(&git_upstream.stdout)
        .trim()
        .to_owned();
    assert_eq!(expected, "refs/custom/origin/main");

    let repo = open(dir.path());
    let rows = list_branches(
        &repo,
        &BranchListQuery {
            scope: BranchScope::Local,
            refs: RefQuery::default(),
        },
        BranchDetail::Full,
    )
    .expect("list");
    let topic = rows.iter().find(|r| r.name == "topic").expect("topic row");
    let up = topic.upstream.as_ref().expect("upstream");
    assert_eq!(up.full_ref, expected);
    assert!(!up.gone);
}

#[test]
fn subject_preserves_whitespace_like_git() {
    let dir = TempDir::new().expect("tempdir");
    git(dir.path(), &["init", "-q", "-b", "main"]);
    std::fs::write(dir.path().join("f"), b"x").unwrap();
    git(dir.path(), &["add", "f"]);
    let msg_path = dir.path().join("msg.txt");
    std::fs::write(
        &msg_path,
        "  hello   world\tthere  \n  next\t line  \n\nbody\n",
    )
    .unwrap();
    git(dir.path(), &["commit", "-F", msg_path.to_str().unwrap()]);

    let git_subject = Command::new("git")
        .current_dir(dir.path())
        .args([
            "for-each-ref",
            "--format=%(contents:subject)",
            "refs/heads/main",
        ])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git");
    assert!(git_subject.status.success());
    let expected = String::from_utf8_lossy(&git_subject.stdout);
    let expected = expected.strip_suffix('\n').unwrap_or(&expected);

    let repo = open(dir.path());
    let rows = list_branches(
        &repo,
        &BranchListQuery {
            scope: BranchScope::Local,
            refs: RefQuery::default(),
        },
        BranchDetail::Full,
    )
    .expect("list");
    assert_eq!(rows[0].subject.as_deref(), Some(expected));
}

#[test]
fn empty_commit_message_subject_is_empty() {
    let dir = TempDir::new().expect("tempdir");
    git(dir.path(), &["init", "-q", "-b", "main"]);
    std::fs::write(dir.path().join("f"), b"x").unwrap();
    git(dir.path(), &["add", "f"]);
    git(dir.path(), &["commit", "--allow-empty-message", "-m", ""]);

    let git_subject = Command::new("git")
        .current_dir(dir.path())
        .args([
            "for-each-ref",
            "--format=%(contents:subject)",
            "refs/heads/main",
        ])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git");
    assert!(git_subject.status.success());
    let expected = String::from_utf8_lossy(&git_subject.stdout);
    let expected = expected.strip_suffix('\n').unwrap_or(&expected);
    assert!(expected.is_empty());

    let repo = open(dir.path());
    let rows = list_branches(
        &repo,
        &BranchListQuery {
            scope: BranchScope::Local,
            refs: RefQuery::default(),
        },
        BranchDetail::Full,
    )
    .expect("list");
    assert_eq!(rows[0].subject.as_deref(), Some(""));
}

#[test]
fn multiline_subject_matches_git_contents_subject() {
    let dir = TempDir::new().expect("tempdir");
    git(dir.path(), &["init", "-q", "-b", "main"]);
    std::fs::write(dir.path().join("f"), b"x").unwrap();
    git(dir.path(), &["add", "f"]);
    let msg_path = dir.path().join("msg.txt");
    std::fs::write(&msg_path, "hello\nworld\n\nbody").unwrap();
    git(dir.path(), &["commit", "-F", msg_path.to_str().unwrap()]);

    let git_subject = Command::new("git")
        .current_dir(dir.path())
        .args([
            "for-each-ref",
            "--format=%(contents:subject)",
            "refs/heads/main",
        ])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git");
    assert!(git_subject.status.success());
    let expected = String::from_utf8_lossy(&git_subject.stdout)
        .trim()
        .to_owned();
    assert_eq!(expected, "hello world");

    let repo = open(dir.path());
    let rows = list_branches(
        &repo,
        &BranchListQuery {
            scope: BranchScope::Local,
            refs: RefQuery::default(),
        },
        BranchDetail::Full,
    )
    .expect("list");
    assert_eq!(rows[0].subject.as_deref(), Some("hello world"));
}

#[test]
fn names_detail_rejects_date_sort_without_reading_objects() {
    let dir = TempDir::new().expect("tempdir");
    git(dir.path(), &["init", "-q", "-b", "main"]);
    std::fs::write(dir.path().join("f"), b"x").unwrap();
    git(dir.path(), &["add", "f"]);
    git(dir.path(), &["commit", "-qm", "c1"]);
    let oid_hex = Command::new("git")
        .current_dir(dir.path())
        .args(["rev-parse", "HEAD"])
        .output()
        .expect("rev-parse");
    let oid_hex = String::from_utf8_lossy(&oid_hex.stdout).trim().to_owned();
    let loose = dir
        .path()
        .join(".git/objects")
        .join(&oid_hex[..2])
        .join(&oid_hex[2..]);
    std::fs::remove_file(&loose).expect("remove loose object");

    let repo = open(dir.path());
    let name_query = BranchListQuery {
        scope: BranchScope::Local,
        refs: RefQuery::default(),
    };
    list_branches(&repo, &name_query, BranchDetail::Names).expect("name sort lists ref");

    let date_query = BranchListQuery {
        scope: BranchScope::Local,
        refs: RefQuery {
            sort: RefSort {
                key: SortKey::Date,
                reverse: false,
            },
            ..RefQuery::default()
        },
    };
    let err = list_branches(&repo, &date_query, BranchDetail::Names).unwrap_err();
    assert!(
        matches!(err, Error::BranchListNamesDateSort),
        "expected typed sort error, got {err:?}"
    );
}
