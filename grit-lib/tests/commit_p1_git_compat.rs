//! Commit P1 flags vs the system `git` binary (trailers, dates, reuse, signoff).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::process::Command;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::porcelain::add::{stage, StageOptions};
use grit_lib::porcelain::commit::{create_commit, CommitRequest};
use grit_lib::progress::NullProgress;
use grit_lib::repo::Repository;
use grit_test_support::git;

fn ident() -> String {
    "P1 Test <p1@example.com> 1700000000 +0000".to_owned()
}

fn commit_req(message: &str) -> CommitRequest {
    let id = ident();
    {
        let mut o = CommitRequest::default();
        o.message = message.to_owned();
        o.author = Some(id.parse().expect("signature"));
        o.committer = Some(id.parse().expect("signature"));
        o
    }
}

fn git_out(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .expect("spawn git");
    assert!(
        out.status.success(),
        "git {:?}: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

fn git_commit(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .current_dir(dir)
        .arg("commit")
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "P1")
        .env("GIT_AUTHOR_EMAIL", "p1@example.com")
        .env("GIT_COMMITTER_NAME", "P1")
        .env("GIT_COMMITTER_EMAIL", "p1@example.com")
        .output()
        .expect("spawn git commit");
    assert!(
        out.status.success(),
        "git commit {:?}: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
}

fn init_repo(root: &Path) -> Repository {
    git(root, &["init", "-q", "-b", "main", "."]);
    git(root, &["config", "grit.changeIds", "false"]);
    git(root, &["config", "user.name", "P1"]);
    git(root, &["config", "user.email", "p1@example.com"]);
    Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open")
}

#[test]
fn grit_commit_signoff_and_trailer_body_matches_git() {
    let grit_dir = tempfile::tempdir().expect("tempdir");
    let git_dir = tempfile::tempdir().expect("tempdir");
    let grit_root = grit_dir.path();
    let git_root = git_dir.path();

    for root in [grit_root, git_root] {
        std::fs::write(root.join("f"), b"x\n").unwrap();
        git(root, &["init", "-q", "-b", "main", "."]);
        git(root, &["config", "grit.changeIds", "false"]);
        git(root, &["config", "user.name", "P1"]);
        git(root, &["config", "user.email", "p1@example.com"]);
        git(root, &["add", "f"]);
    }

    git_commit(
        git_root,
        &[
            "-m",
            "subject",
            "-s",
            "--trailer",
            "Acked-by: Alice",
            "--no-verify",
        ],
    );

    let repo = init_repo(grit_root);
    stage(
        &repo,
        &StageOptions::default(),
        &mut NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .unwrap();
    let mut req = commit_req("subject\n");
    req.author = Some(
        "P1 <p1@example.com> 1700000000 +0000"
            .parse()
            .expect("signature"),
    );
    req.committer = req.author.clone();
    req.signoff = true;
    req.trailers = vec![("Acked-by".to_owned(), "Alice".to_owned())];
    req.hooks = grit_lib::porcelain::commit::HookPolicy::SkipVerify;
    create_commit(
        &repo,
        &req,
        &mut NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .unwrap();

    let grit_body = git_out(grit_root, &["log", "-1", "--format=%B"]);
    let git_body = git_out(git_root, &["log", "-1", "--format=%B"]);
    assert_eq!(
        grit_body, git_body,
        "commit bodies must match Git for -s plus --trailer"
    );
    git_out(grit_root, &["fsck", "--strict"]);
}

#[test]
fn grit_commit_amend_reuse_message_matches_git() {
    let grit_dir = tempfile::tempdir().expect("tempdir");
    let git_dir = tempfile::tempdir().expect("tempdir");
    let grit_root = grit_dir.path();
    let git_root = git_dir.path();

    for root in [grit_root, git_root] {
        git(root, &["init", "-q", "-b", "main", "."]);
        git(root, &["config", "grit.changeIds", "false"]);
        git(root, &["config", "user.name", "P1"]);
        git(root, &["config", "user.email", "p1@example.com"]);
        std::fs::write(root.join("f"), b"1\n").unwrap();
        git(root, &["add", "f"]);
        git_commit(
            root,
            &[
                "-m",
                "source",
                "--author",
                "Source Author <author@example.com>",
                "--date",
                "2001-01-01T00:00:00Z",
                "--no-verify",
            ],
        );
        std::fs::write(root.join("f"), b"2\n").unwrap();
        git(root, &["add", "f"]);
        git_commit(
            root,
            &[
                "-m",
                "tip",
                "--author",
                "Tip Author <tip@example.com>",
                "--no-verify",
            ],
        );
    }

    let source_rev = git_out(git_root, &["rev-parse", "HEAD~1"]);
    std::fs::write(git_root.join("f"), b"3\n").unwrap();
    git(git_root, &["add", "f"]);
    git_commit(git_root, &["--amend", "-C", &source_rev, "--no-verify"]);

    let repo = Repository::discover(Some(grit_root), &RepositoryOptions::empty()).unwrap();
    let source_oid =
        grit_lib::objects::ObjectId::from_hex(&git_out(grit_root, &["rev-parse", "HEAD~1"]))
            .expect("source oid");
    std::fs::write(grit_root.join("f"), b"3\n").unwrap();
    stage(
        &repo,
        &StageOptions::default(),
        &mut NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .unwrap();
    let mut req = commit_req("");
    req.amend = true;
    req.author = None;
    req.reuse_message_from = Some(source_oid);
    req.hooks = grit_lib::porcelain::commit::HookPolicy::SkipVerify;
    create_commit(
        &repo,
        &req,
        &mut NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .unwrap();

    let grit_meta = git_out(grit_root, &["log", "-1", "--format=%an|%ae|%at|%B"]);
    let git_meta = git_out(git_root, &["log", "-1", "--format=%an|%ae|%at|%B"]);
    assert_eq!(
        grit_meta, git_meta,
        "amend -C must match Git author and body"
    );
    git_out(grit_root, &["fsck", "--strict"]);
}

#[test]
fn grit_commit_date_and_author_match_git_log() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let repo = init_repo(root);
    std::fs::write(root.join("f"), b"x\n").unwrap();
    stage(
        &repo,
        &StageOptions::default(),
        &mut NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .unwrap();
    let mut req = commit_req("dated commit\n");
    req.author = Some(
        "Author Person <author@example.com> 1 +0000"
            .parse()
            .expect("signature"),
    );
    req.author_date = Some("946684800 +0000".to_owned());
    create_commit(
        &repo,
        &req,
        &mut NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .unwrap();

    git_out(root, &["fsck", "--strict"]);
    let at = git_out(root, &["log", "-1", "--format=%at"]);
    assert_eq!(at, "946684800");
    let an = git_out(root, &["log", "-1", "--format=%an"]);
    assert_eq!(an, "Author Person");
}

#[test]
fn grit_commit_reuse_message_matches_git_c() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let repo = init_repo(root);
    std::fs::write(root.join("f"), b"1\n").unwrap();
    stage(
        &repo,
        &StageOptions::default(),
        &mut NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .unwrap();
    let mut first = commit_req("original subject\n\nbody\n");
    first.author = Some(
        "Orig Author <orig@example.com> 123 +0000"
            .parse()
            .expect("signature"),
    );
    let first_out = create_commit(
        &repo,
        &first,
        &mut NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .unwrap();

    std::fs::write(root.join("f"), b"2\n").unwrap();
    stage(
        &repo,
        &StageOptions::default(),
        &mut NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .unwrap();
    let mut req = commit_req("");
    req.author = None;
    req.reuse_message_from = Some(first_out.oid);
    create_commit(
        &repo,
        &req,
        &mut NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .unwrap();

    git_out(root, &["fsck", "--strict"]);
    let line = git_out(root, &["log", "-1", "--format=%an|%B"]);
    let (an, body) = line.split_once('|').expect("pipe");
    assert_eq!(an, "Orig Author");
    assert!(body.contains("original subject"));
    assert!(body.contains("body"));
}
