//! `continue_merge` and cross-tool merge conclusion (system `git`).

use std::path::Path;
use std::process::Command;

use grit_lib::change_id::change_id;
use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::{parse_commit, ObjectId};
use grit_lib::porcelain::merge::{
    continue_merge, integrate_branch, ContinueMergeError, ContinueOptions, IntegrateBranchOutcome,
};
use grit_lib::progress::NullProgress;
use grit_lib::repo::Repository;
use grit_test_support::git;

fn ident() -> String {
    "Merge Test <merge@example.com> 1700000000 +0000".to_owned()
}

fn continue_options(message: Option<&str>) -> ContinueOptions {
    let id = ident();
    ContinueOptions {
        message: message.map(str::to_owned),
        author: Some(id.parse().expect("signature")),
        committer: Some(id.parse().expect("signature")),
    }
}

fn git_out(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "Merge Test")
        .env("GIT_AUTHOR_EMAIL", "merge@example.com")
        .env("GIT_COMMITTER_NAME", "Merge Test")
        .env("GIT_COMMITTER_EMAIL", "merge@example.com")
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

fn init_conflict_merge(root: &Path) -> Repository {
    git(root, &["init", "-q", "-b", "main", "."]);
    git(root, &["config", "user.name", "T"]);
    git(root, &["config", "user.email", "t@e.com"]);
    git(root, &["config", "core.logAllRefUpdates", "true"]);

    std::fs::write(root.join("f"), b"base\n").unwrap();
    git(root, &["add", "f"]);
    git(root, &["commit", "-qm", "base"]);

    git(root, &["checkout", "-qb", "side"]);
    std::fs::write(root.join("f"), b"side\n").unwrap();
    git(root, &["commit", "-qam", "side"]);

    git(root, &["checkout", "-q", "main"]);
    std::fs::write(root.join("f"), b"main\n").unwrap();
    git(root, &["commit", "-qam", "main"]);

    let merge_out = Command::new("git")
        .current_dir(root)
        .args(["merge", "side"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git merge");
    assert!(!merge_out.status.success(), "expected merge conflict");

    Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open grit repo")
}

fn init_grit_two_file_conflict_merge(root: &Path) -> Repository {
    git(root, &["init", "-q", "-b", "main", "."]);
    git(root, &["config", "user.name", "T"]);
    git(root, &["config", "user.email", "t@e.com"]);
    git(root, &["config", "core.logAllRefUpdates", "true"]);

    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("repo");
    std::fs::write(root.join("a.txt"), b"1\n2\n3\n").unwrap();
    std::fs::write(root.join("b.txt"), b"keep\n").unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "-qm", "base"]);

    git(root, &["checkout", "-qb", "topic"]);
    std::fs::write(root.join("a.txt"), b"1\ntopic\n3\n").unwrap();
    std::fs::write(root.join("b.txt"), b"tb\n").unwrap();
    git(root, &["commit", "-qam", "topic"]);
    let topic: ObjectId = git_out(root, &["rev-parse", "topic"])
        .parse()
        .expect("topic");

    git(root, &["checkout", "-q", "main"]);
    std::fs::write(root.join("a.txt"), b"1\nmain\n3\n").unwrap();
    std::fs::write(root.join("b.txt"), b"mb\n").unwrap();
    git(root, &["commit", "-qam", "main"]);
    let main: ObjectId = git_out(root, &["rev-parse", "HEAD"]).parse().expect("main");

    let outcome = integrate_branch(&repo, "refs/heads/main", main, topic, "topic").expect("merge");
    assert!(matches!(outcome, IntegrateBranchOutcome::Conflicts { .. }));
    repo
}

fn init_grit_conflict_merge(root: &Path) -> Repository {
    git(root, &["init", "-q", "-b", "main", "."]);
    git(root, &["config", "user.name", "T"]);
    git(root, &["config", "user.email", "t@e.com"]);
    git(root, &["config", "core.logAllRefUpdates", "true"]);
    git(root, &["config", "grit.changeIds", "true"]);

    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("repo");
    std::fs::write(root.join("f"), b"base\n").unwrap();
    git(root, &["add", "f"]);
    git(root, &["commit", "-qm", "base"]);

    git(root, &["checkout", "-qb", "side"]);
    std::fs::write(root.join("f"), b"side\n").unwrap();
    git(root, &["commit", "-qam", "side"]);
    let side: ObjectId = git_out(root, &["rev-parse", "side"]).parse().expect("side");

    git(root, &["checkout", "-q", "main"]);
    std::fs::write(root.join("f"), b"main\n").unwrap();
    git(root, &["commit", "-qam", "main"]);
    let main: ObjectId = git_out(root, &["rev-parse", "HEAD"]).parse().expect("main");

    let outcome = integrate_branch(&repo, "refs/heads/main", main, side, "side").expect("merge");
    assert!(matches!(outcome, IntegrateBranchOutcome::Conflicts { .. }));
    repo
}

#[test]
fn grit_conflict_then_grit_continue_fsck_clean() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let repo = init_grit_conflict_merge(root);

    std::fs::write(root.join("f"), b"resolved\n").unwrap();
    git(root, &["add", "f"]);

    let outcome =
        continue_merge(&repo, &continue_options(None), &mut NullProgress).expect("continue");
    assert!(!repo.git_dir().join("MERGE_HEAD").exists());

    let fsck = Command::new("git")
        .current_dir(root)
        .args(["fsck", "--strict"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("fsck");
    assert!(
        fsck.status.success(),
        "git fsck: {}",
        String::from_utf8_lossy(&fsck.stderr)
    );

    let parents_line = git_out(root, &["rev-list", "--parents", "-1", "HEAD"]);
    let parts: Vec<&str> = parents_line.split_whitespace().collect();
    assert_eq!(parts.len(), 3, "merge commit: {parents_line}");
    assert_eq!(parts[0], outcome.oid.to_hex());
}

#[test]
fn git_merge_conflict_then_grit_continue() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let repo = init_conflict_merge(root);

    std::fs::write(root.join("f"), b"resolved\n").unwrap();
    git(root, &["add", "f"]);

    let side_tip = ObjectId::from_hex(&git_out(root, &["rev-parse", "side"])).expect("side");
    let main_before = ObjectId::from_hex(&git_out(root, &["rev-parse", "HEAD"])).expect("main");

    continue_merge(&repo, &continue_options(None), &mut NullProgress).expect("continue");

    let parents_line = git_out(root, &["rev-list", "--parents", "-1", "HEAD"]);
    let parts: Vec<&str> = parents_line.split_whitespace().collect();
    assert_eq!(parts.len(), 3, "parents: {parents_line}");
    assert_eq!(parts[1], main_before.to_hex());
    assert_eq!(parts[2], side_tip.to_hex());

    let msg = git_out(root, &["log", "-1", "--format=%B"]);
    assert!(
        msg.starts_with("Merge branch 'side'"),
        "message from MERGE_MSG: {msg:?}"
    );
}

#[test]
fn grit_conflict_then_git_commit_no_edit() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let repo = init_grit_conflict_merge(root);

    std::fs::write(root.join("f"), b"resolved\n").unwrap();
    git(root, &["add", "f"]);

    let side_tip = ObjectId::from_hex(&git_out(root, &["rev-parse", "side"])).expect("side");
    let main_before = ObjectId::from_hex(&git_out(root, &["rev-parse", "HEAD"])).expect("main");

    git_out(root, &["commit", "--no-edit"]);

    assert!(!repo.git_dir().join("MERGE_HEAD").exists());
    let parents_line = git_out(root, &["rev-list", "--parents", "-1", "HEAD"]);
    let parts: Vec<&str> = parents_line.split_whitespace().collect();
    assert_eq!(parts[1], main_before.to_hex());
    assert_eq!(parts[2], side_tip.to_hex());
}

#[test]
fn continue_after_edit_without_add_auto_stages_resolved_paths() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let repo = init_grit_two_file_conflict_merge(root);

    std::fs::write(root.join("a.txt"), b"1\nboth\n3\n").unwrap();
    std::fs::write(root.join("b.txt"), b"resolved\n").unwrap();

    let topic_tip = ObjectId::from_hex(&git_out(root, &["rev-parse", "topic"])).expect("topic");
    let main_before = ObjectId::from_hex(&git_out(root, &["rev-parse", "HEAD"])).expect("main");

    continue_merge(&repo, &continue_options(None), &mut NullProgress).expect("continue");

    assert!(!repo.git_dir().join("MERGE_HEAD").exists());
    let parents_line = git_out(root, &["rev-list", "--parents", "-1", "HEAD"]);
    let parts: Vec<&str> = parents_line.split_whitespace().collect();
    assert_eq!(parts.len(), 3, "merge commit: {parents_line}");
    assert_eq!(parts[1], main_before.to_hex());
    assert_eq!(parts[2], topic_tip.to_hex());
}

#[test]
fn continue_refuses_unmerged_paths() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let repo = init_conflict_merge(root);

    let err = continue_merge(&repo, &continue_options(None), &mut NullProgress).unwrap_err();
    assert!(matches!(
        err,
        ContinueMergeError::UnmergedPaths { ref paths } if paths == &["f"]
    ));
    assert!(repo.git_dir().join("MERGE_HEAD").exists());
}

#[test]
fn continue_without_merge_errors() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "user.name", "T"]);
    git(root, &["config", "user.email", "t@e.com"]);
    std::fs::write(root.join("f"), b"x\n").unwrap();
    git(root, &["add", "f"]);
    git(root, &["commit", "-qm", "one"]);

    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("repo");
    let err = continue_merge(&repo, &continue_options(None), &mut NullProgress).unwrap_err();
    assert!(matches!(err, ContinueMergeError::NoMergeInProgress));
}

#[test]
fn continue_stamps_change_id() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let repo = init_grit_conflict_merge(root);

    std::fs::write(root.join("f"), b"resolved\n").unwrap();
    git(root, &["add", "f"]);

    let outcome =
        continue_merge(&repo, &continue_options(None), &mut NullProgress).expect("continue");
    let obj = repo.odb().read(&outcome.oid).expect("commit");
    let parsed = parse_commit(&obj.data).expect("parse");
    let stored = change_id(&parsed).expect("change-id header");
    assert_eq!(outcome.change_id.as_ref(), Some(&stored));
}
