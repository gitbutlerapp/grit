//! Commit P1 flags vs the system `git` binary (trailers, dates, reuse, signoff).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::process::Command;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::parse_commit;
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
    CommitRequest {
        message: message.to_owned(),
        author: id.clone(),
        committer: id,
        ..Default::default()
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

fn init_repo(root: &Path) -> Repository {
    git(root, &["init", "-q", "-b", "main", "."]);
    git(root, &["config", "grit.changeIds", "false"]);
    git(root, &["config", "user.name", "P1"]);
    git(root, &["config", "user.email", "p1@example.com"]);
    Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open")
}

#[test]
fn grit_commit_trailers_match_git_interpret_trailers() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let repo = init_repo(root);
    std::fs::write(root.join("f"), b"x\n").unwrap();
    stage(&repo, &StageOptions::default(), &mut NullProgress).unwrap();
    let mut req = commit_req("subject\n");
    req.trailers = vec![("Reviewed-by".to_owned(), "Reviewer".to_owned())];
    create_commit(&repo, &req, &mut NullProgress).unwrap();

    git_out(root, &["fsck", "--strict"]);
    let body = git_out(root, &["log", "-1", "--format=%B"]);
    assert!(body.contains("Reviewed-by: Reviewer"));
    let mut child = Command::new("git")
        .current_dir(root)
        .args(["interpret-trailers", "--parse"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("spawn git");
    use std::io::Write;
    {
        let stdin = child.stdin.as_mut().expect("stdin");
        stdin.write_all(body.as_bytes()).expect("write stdin");
    }
    let out = child.wait_with_output().expect("wait");
    assert!(out.status.success());
    let parse = String::from_utf8_lossy(&out.stdout);
    assert!(parse.contains("Reviewed-by: Reviewer"));
}

#[test]
fn grit_commit_date_and_author_match_git_log() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let repo = init_repo(root);
    std::fs::write(root.join("f"), b"x\n").unwrap();
    stage(&repo, &StageOptions::default(), &mut NullProgress).unwrap();
    let mut req = commit_req("dated commit\n");
    req.author = "Author Person <author@example.com> 1 +0000".to_owned();
    req.author_date = Some("946684800 +0000".to_owned());
    create_commit(&repo, &req, &mut NullProgress).unwrap();

    git_out(root, &["fsck", "--strict"]);
    let at = git_out(root, &["log", "-1", "--format=%at"]);
    assert_eq!(at, "946684800");
    let an = git_out(root, &["log", "-1", "--format=%an"]);
    assert_eq!(an, "Author Person");
}

#[test]
fn grit_commit_signoff_matches_git() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let repo = init_repo(root);
    std::fs::write(root.join("f"), b"x\n").unwrap();
    stage(&repo, &StageOptions::default(), &mut NullProgress).unwrap();
    let mut req = commit_req("signoff subject\n");
    req.committer = "Sig User <sig@example.com> 1700000000 +0000".to_owned();
    req.signoff = true;
    create_commit(&repo, &req, &mut NullProgress).unwrap();

    git_out(root, &["fsck", "--strict"]);
    let body = git_out(root, &["log", "-1", "--format=%B"]);
    assert!(body.contains("Signed-off-by: Sig User <sig@example.com>"));
}

#[test]
fn grit_commit_reuse_message_matches_git_c() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let repo = init_repo(root);
    std::fs::write(root.join("f"), b"1\n").unwrap();
    stage(&repo, &StageOptions::default(), &mut NullProgress).unwrap();
    let mut first = commit_req("original subject\n\nbody\n");
    first.author = "Orig Author <orig@example.com> 123 +0000".to_owned();
    let first_out = create_commit(&repo, &first, &mut NullProgress).unwrap();

    std::fs::write(root.join("f"), b"2\n").unwrap();
    stage(&repo, &StageOptions::default(), &mut NullProgress).unwrap();
    let mut req = commit_req("");
    req.author = String::new();
    req.reuse_message_from = Some(first_out.oid);
    create_commit(&repo, &req, &mut NullProgress).unwrap();

    git_out(root, &["fsck", "--strict"]);
    let (an, body) = {
        let line = git_out(root, &["log", "-1", "--format=%an|%B"]);
        let (an, body) = line.split_once('|').expect("pipe");
        (an.to_owned(), body.to_owned())
    };
    assert_eq!(an, "Orig Author");
    assert!(body.contains("original subject"));
    assert!(body.contains("body"));
}
