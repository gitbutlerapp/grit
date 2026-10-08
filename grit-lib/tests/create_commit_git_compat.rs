//! Round-trip `create_commit` with the system `git` binary.

use std::path::Path;
use std::process::Command;

use grit_lib::objects::{parse_commit, ObjectId};
use grit_lib::porcelain::add::{stage, StageOptions};
use grit_lib::porcelain::commit::{create_commit, CommitRequest};
use grit_lib::progress::NullProgress;
use grit_lib::repo::Repository;
use grit_test_support::git;

fn ident() -> String {
    "Compat Test <compat@example.com> 1700000000 +0000".to_owned()
}

fn commit_req(message: &str) -> CommitRequest {
    let id = ident();
    CommitRequest {
        message: message.to_owned(),
        author: id.clone(),
        committer: id,
        allow_empty: false,
    }
}

fn git_out(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "Compat Test")
        .env("GIT_AUTHOR_EMAIL", "compat@example.com")
        .env("GIT_COMMITTER_NAME", "Compat Test")
        .env("GIT_COMMITTER_EMAIL", "compat@example.com")
        .output()
        .expect("spawn git");
    assert!(
        out.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

fn fsck(dir: &Path) {
    let out = Command::new("git")
        .current_dir(dir)
        .args(["fsck", "--strict"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git fsck");
    assert!(
        out.status.success(),
        "git fsck: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn grit_first_commit_subdirs_index_sorted_for_git() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main", "."]);
    git(root, &["config", "core.logAllRefUpdates", "true"]);

    std::fs::write(root.join("a.txt"), b"1\n").unwrap();
    std::fs::create_dir_all(root.join("d")).unwrap();
    std::fs::write(root.join("d/x.txt"), b"2\n").unwrap();
    std::fs::write(root.join("z.txt"), b"3\n").unwrap();

    let repo = Repository::discover(Some(root)).expect("open");
    stage(&repo, &StageOptions::default(), &mut NullProgress).expect("stage");
    create_commit(&repo, &commit_req("base"), &mut NullProgress).expect("commit");

    let staged = git_out(root, &["ls-files", "--stage"]);
    let paths: Vec<&str> = staged
        .lines()
        .map(|line| line.split_whitespace().nth(3).expect("path"))
        .collect();
    assert_eq!(paths, ["a.txt", "d/x.txt", "z.txt"]);

    assert!(
        git_out(root, &["status", "--short"]).is_empty(),
        "git status should be clean"
    );
    fsck(root);
}

#[test]
fn grit_create_commit_initial_validated_by_git() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    std::fs::create_dir_all(root).unwrap();
    git(root, &["init", "-q", "-b", "main", "."]);
    git(root, &["config", "core.logAllRefUpdates", "true"]);

    std::fs::write(root.join("README.md"), b"# hi\n").unwrap();
    let repo = Repository::discover(Some(root)).expect("open");
    stage(&repo, &StageOptions::default(), &mut NullProgress).expect("stage");
    let outcome =
        create_commit(&repo, &commit_req("initial grit"), &mut NullProgress).expect("commit");

    fsck(root);
    let show = git_out(root, &["show", "-s", "--format=%H %s", "HEAD"]);
    assert!(show.contains(&outcome.oid.to_hex()));
    assert!(show.contains("initial grit"));

    assert_eq!(
        git_out(root, &["rev-parse", "refs/heads/main"]),
        outcome.oid.to_hex()
    );

    let head_reflog = git_out(root, &["reflog", "show", "-1", "HEAD"]);
    assert!(
        head_reflog.contains("initial grit"),
        "HEAD reflog: {head_reflog:?}"
    );
}

#[test]
fn grit_create_commit_second_validated_by_git() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main", "."]);
    git(root, &["config", "core.logAllRefUpdates", "true"]);
    std::fs::write(root.join("a.txt"), b"1\n").unwrap();
    git(root, &["add", "a.txt"]);
    git(root, &["commit", "-q", "-m", "seed"]);

    std::fs::write(root.join("a.txt"), b"2\n").unwrap();
    let repo = Repository::discover(Some(root)).expect("open");
    stage(&repo, &StageOptions::default(), &mut NullProgress).expect("stage");
    let outcome =
        create_commit(&repo, &commit_req("second grit"), &mut NullProgress).expect("commit");

    fsck(root);
    assert_eq!(git_out(root, &["rev-parse", "HEAD"]), outcome.oid.to_hex());
    let parent = git_out(root, &["rev-parse", "HEAD^"]);
    assert_ne!(parent, outcome.oid.to_hex());
}

#[test]
fn git_commits_readable_by_grit_create_commit_chain() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main", "."]);
    git(root, &["config", "core.logAllRefUpdates", "true"]);
    std::fs::write(root.join("f.txt"), b"v1\n").unwrap();
    git(root, &["add", "f.txt"]);
    git(root, &["commit", "-q", "-m", "from git"]);

    std::fs::write(root.join("f.txt"), b"v2\n").unwrap();
    let repo = Repository::discover(Some(root)).expect("open");
    stage(&repo, &StageOptions::default(), &mut NullProgress).expect("stage");
    let outcome =
        create_commit(&repo, &commit_req("from grit"), &mut NullProgress).expect("commit");

    fsck(root);
    let git_head = ObjectId::from_hex(&git_out(root, &["rev-parse", "HEAD"])).expect("oid");
    assert_eq!(git_head, outcome.oid);

    let obj = repo.odb.read(&outcome.oid).expect("read commit");
    let parsed = parse_commit(&obj.data).expect("parse");
    assert!(parsed.message.contains("from grit"));
}
