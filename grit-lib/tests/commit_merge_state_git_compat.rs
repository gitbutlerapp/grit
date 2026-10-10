//! Merge-in-progress commit behavior checked against the system `git` binary.

use std::fs;
use std::path::Path;
use std::process::Command;

use grit_lib::error::Error;
use grit_lib::objects::{parse_commit, ObjectId};
use grit_lib::porcelain::commit::{create_commit, CommitRequest};
use grit_lib::progress::NullProgress;
use grit_lib::repo::Repository;
use grit_test_support::git;

fn ident() -> String {
    "Merge Test <merge@example.com> 1700000000 +0000".to_owned()
}

fn commit_req(message: &str) -> CommitRequest {
    let id = ident();
    CommitRequest {
        message: message.to_owned(),
        author: id.clone(),
        committer: id,
        allow_empty: false,
        sign_override: None,
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

fn git_fails(dir: &Path, args: &[&str]) -> String {
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
        !out.status.success(),
        "git {:?} unexpectedly succeeded",
        args
    );
    String::from_utf8_lossy(&out.stderr).into_owned()
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

    Repository::discover(Some(root)).expect("open grit repo")
}

#[test]
fn create_commit_rejects_unmerged_index_like_git() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let repo = init_conflict_merge(root);

    let index = repo.load_index().expect("index");
    assert!(
        index.has_unmerged_entries(),
        "git merge should leave unmerged index entries"
    );
    assert!(
        repo.git_dir.join("MERGE_HEAD").exists(),
        "MERGE_HEAD should remain during conflict"
    );

    let git_stderr = git_fails(root, &["commit", "-m", "oops"]);
    assert!(
        git_stderr.contains("unmerged"),
        "git should refuse unmerged commit: {git_stderr}"
    );

    let err = create_commit(&repo, &commit_req("oops"), &mut NullProgress).unwrap_err();
    assert!(
        matches!(err, Error::IndexUnmerged),
        "grit should refuse unmerged commit: {err}"
    );

    let index_after = repo.load_index().expect("index");
    assert!(
        index_after.has_unmerged_entries(),
        "refusal must not clear unmerged stages"
    );
    assert!(
        repo.git_dir.join("MERGE_HEAD").exists(),
        "MERGE_HEAD must remain after refused commit"
    );
}

#[test]
fn create_commit_concludes_resolved_merge_with_two_parents() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let repo = init_conflict_merge(root);

    std::fs::write(root.join("f"), b"resolved\n").unwrap();
    git(root, &["add", "f"]);

    let side_tip = ObjectId::from_hex(&git_out(root, &["rev-parse", "side"])).expect("side oid");
    let main_before = ObjectId::from_hex(&git_out(root, &["rev-parse", "HEAD"])).expect("main oid");

    let outcome =
        create_commit(&repo, &commit_req("merge side"), &mut NullProgress).expect("merge commit");

    assert_eq!(outcome.parent, Some(main_before));

    let parents_line = git_out(root, &["rev-list", "--parents", "-1", "HEAD"]);
    let parts: Vec<&str> = parents_line.split_whitespace().collect();
    assert_eq!(
        parts.len(),
        3,
        "merge commit should have two parents: {parents_line}"
    );
    assert_eq!(parts[1], main_before.to_hex());
    assert_eq!(parts[2], side_tip.to_hex());

    let blob = git_out(root, &["show", "HEAD:f"]);
    assert_eq!(blob, "resolved");

    assert!(
        !repo.git_dir.join("MERGE_HEAD").exists(),
        "MERGE_HEAD should be cleared after merge commit"
    );

    let commit_obj = repo.odb.read(&outcome.oid).expect("commit");
    let parsed = parse_commit(&commit_obj.data).expect("parse");
    assert_eq!(parsed.parents.len(), 2);

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
}

#[test]
fn create_commit_concludes_merge_when_resolved_as_ours() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let repo = init_conflict_merge(root);

    git(root, &["checkout", "--ours", "--", "f"]);
    git(root, &["add", "f"]);

    let side_tip = ObjectId::from_hex(&git_out(root, &["rev-parse", "side"])).expect("side oid");
    let main_before = ObjectId::from_hex(&git_out(root, &["rev-parse", "HEAD"])).expect("main oid");
    let main_tree_before = git_out(root, &["rev-parse", "HEAD^{tree}"]);

    create_commit(&repo, &commit_req("resolve as ours"), &mut NullProgress)
        .expect("merge commit with unchanged tree vs first parent");

    let parents_line = git_out(root, &["rev-list", "--parents", "-1", "HEAD"]);
    let parts: Vec<&str> = parents_line.split_whitespace().collect();
    assert_eq!(
        parts.len(),
        3,
        "merge commit should have two parents like git: {parents_line}"
    );
    assert_eq!(parts[1], main_before.to_hex());
    assert_eq!(parts[2], side_tip.to_hex());

    let tree_at_head = git_out(root, &["rev-parse", "HEAD^{tree}"]);
    assert_eq!(
        tree_at_head, main_tree_before,
        "ours resolution should keep the first-parent tree"
    );
    assert_eq!(git_out(root, &["show", "HEAD:f"]), "main");

    assert!(
        !repo.git_dir.join("MERGE_HEAD").exists(),
        "MERGE_HEAD should be cleared after merge commit"
    );
}

#[test]
fn create_commit_applies_merge_autostash_like_git() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main", "."]);
    git(root, &["config", "user.name", "T"]);
    git(root, &["config", "user.email", "t@e.com"]);
    git(root, &["config", "core.logAllRefUpdates", "true"]);

    std::fs::write(root.join("f"), b"base\n").unwrap();
    std::fs::write(root.join("g"), b"base-g\n").unwrap();
    git(root, &["add", "f", "g"]);
    git(root, &["commit", "-qm", "base"]);

    git(root, &["checkout", "-qb", "side"]);
    std::fs::write(root.join("f"), b"side\n").unwrap();
    git(root, &["commit", "-qam", "side"]);

    git(root, &["checkout", "-q", "main"]);
    std::fs::write(root.join("f"), b"main\n").unwrap();
    git(root, &["commit", "-qam", "main"]);

    std::fs::write(root.join("g"), b"local-g\n").unwrap();
    let merge_out = Command::new("git")
        .current_dir(root)
        .args(["merge", "--autostash", "side"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git merge --autostash");
    assert!(!merge_out.status.success(), "expected merge conflict");
    assert!(
        root.join(".git/MERGE_AUTOSTASH").exists(),
        "merge --autostash should record MERGE_AUTOSTASH"
    );

    git(root, &["checkout", "--ours", "--", "f"]);
    git(root, &["add", "f"]);

    let repo = Repository::discover(Some(root)).expect("open");
    create_commit(
        &repo,
        &commit_req("merge with autostash"),
        &mut NullProgress,
    )
    .expect("merge commit");

    assert!(
        !root.join(".git/MERGE_AUTOSTASH").exists(),
        "MERGE_AUTOSTASH must be removed after merge commit"
    );
    assert_eq!(
        git_out(root, &["show", "HEAD:g"]),
        "base-g",
        "committed tree should still reflect ours for g"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("g")).unwrap(),
        "local-g\n",
        "autostash should restore unstaged local change to g"
    );
    let status = git_out(root, &["status", "--short"]);
    assert!(
        status.contains(" M g") || status.starts_with("M g"),
        "g should be unstaged modified after autostash restore: {status:?}"
    );
}

#[test]
fn create_commit_clears_merge_rerere_state_like_git() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main", "."]);
    git(root, &["config", "user.name", "T"]);
    git(root, &["config", "user.email", "t@e.com"]);
    git(root, &["config", "core.logAllRefUpdates", "true"]);
    git(root, &["config", "rerere.enabled", "true"]);

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
    git(root, &["rerere"]);

    git(root, &["checkout", "--ours", "--", "f"]);
    git(root, &["add", "f"]);

    let repo = Repository::discover(Some(root)).expect("open");
    create_commit(&repo, &commit_req("merge with rerere"), &mut NullProgress)
        .expect("merge commit");

    let merge_rr_len = fs::metadata(root.join(".git/MERGE_RR"))
        .map(|m| m.len())
        .unwrap_or(0);
    assert_eq!(
        merge_rr_len, 0,
        "MERGE_RR should be empty after merge commit like git"
    );
    let rerere_status = git_out(root, &["rerere", "status"]);
    assert!(
        rerere_status.is_empty(),
        "git rerere status should be empty, got: {rerere_status:?}"
    );
}
