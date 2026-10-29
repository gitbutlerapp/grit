//! Issue #2322: legacy tree modes (100664 / 100775) must canonicalize in the index and diffs.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::Write;
use std::path::Path;
use std::process::Command;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::diff::{Diff, RenameDetection, UncommittedDiff};
use grit_lib::error::Error;
use grit_lib::objects::{parse_tree, ObjectId, ObjectKind};
use grit_lib::porcelain::checkout::checkout_between_trees;
use grit_lib::porcelain::commit::{create_commit, CommitRequest};
use grit_lib::porcelain::reset::{reset_to_commit, ResetMode, ResetOptions};
use grit_lib::porcelain::status::{status, StatusOptions};
use grit_lib::progress::NullProgress;
use grit_lib::repo::Repository;
use grit_test_support::{git_hash_object_literally, HashAlgo as FixtureAlgo};

const IDENT: &str = "Test User <t@example.com> 1700000000 +0000";

fn git(root: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(root)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .env("GIT_AUTHOR_DATE", "1700000000 +0000")
        .env("GIT_COMMITTER_DATE", "1700000000 +0000")
        .output()
        .expect("git spawn");
    assert!(
        out.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn write_blob(root: &Path, data: &[u8]) -> ObjectId {
    let mut child = Command::new("git")
        .current_dir(root)
        .args(["hash-object", "-w", "--stdin"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("hash-object");
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(data).expect("stdin");
    }
    let out = child.wait_with_output().expect("hash-object wait");
    assert!(out.status.success());
    ObjectId::from_hex(String::from_utf8_lossy(&out.stdout).trim()).expect("blob oid")
}

fn init_legacy_mode_repo(root: &Path) -> ObjectId {
    git(root, &["init", "-q", "-b", "main", "."]);
    git(root, &["config", "user.email", "a@b.c"]);
    git(root, &["config", "user.name", "A"]);

    let blob = write_blob(root, b"hi");
    let blob_bytes = blob.as_bytes();

    let mut tree_body = b"100664 a.txt\0".to_vec();
    tree_body.extend_from_slice(blob_bytes);
    tree_body.extend_from_slice(b"100775 z.txt\0");
    tree_body.extend_from_slice(blob_bytes);

    let tree_hex =
        git_hash_object_literally(root, FixtureAlgo::Sha1, "tree", &tree_body).expect("tree");
    let tree = ObjectId::from_hex(tree_hex.trim()).expect("tree oid");

    let commit_hex = git(
        root,
        &["commit-tree", tree_hex.trim(), "-m", "legacy-modes"],
    )
    .trim()
    .to_string();
    git(root, &["update-ref", "refs/heads/main", &commit_hex]);
    git(root, &["symbolic-ref", "HEAD", "refs/heads/main"]);
    git(root, &["checkout", "-q", "-f", "main"]);

    tree
}

fn open_repo(root: &Path) -> Repository {
    Repository::open(
        &root.join(".git"),
        &RepositoryOptions::empty().with_work_tree(root),
    )
    .expect("open")
}

fn assert_index_modes_canonical(root: &Path) {
    let ls = git(root, &["ls-files", "-s"]);
    assert!(
        !ls.contains("100664") && !ls.contains("100775"),
        "index must not store legacy modes: {ls}"
    );
    let diff_index = Command::new("git")
        .current_dir(root)
        .args(["diff-index", "HEAD"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .status()
        .expect("diff-index");
    assert!(diff_index.success(), "git diff-index HEAD must succeed");
}

#[test]
fn legacy_tree_modes_status_and_commit_clean_after_git_checkout() {
    let root = tempfile::tempdir().expect("tempdir");
    init_legacy_mode_repo(root.path());

    let head_before = git(root.path(), &["rev-parse", "HEAD"]).trim().to_string();

    let repo = open_repo(root.path());
    let model = status(
        &repo,
        &StatusOptions::default(),
        &mut NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("status");
    assert!(
        model.is_clean(),
        "expected clean status, got staged={:?} unstaged={:?}",
        model.staged,
        model.unstaged
    );

    let req = CommitRequest {
        message: "nochange".to_owned(),
        author: Some(IDENT.parse().expect("author")),
        committer: Some(IDENT.parse().expect("committer")),
        ..Default::default()
    };
    let err = create_commit(
        &repo,
        &req,
        &mut NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect_err("commit");
    assert!(
        matches!(err, Error::NothingToCommit),
        "expected NothingToCommit, got {err}"
    );

    let head_after = git(root.path(), &["rev-parse", "HEAD"]).trim().to_string();
    assert_eq!(head_before, head_after, "git log must be unchanged");
}

#[test]
fn legacy_tree_modes_worktree_diff_empty_after_git_checkout() {
    let root = tempfile::tempdir().expect("tempdir");
    init_legacy_mode_repo(root.path());
    let repo = open_repo(root.path());

    let head = repo.head_commit().expect("head commit");
    let head_tree = repo.odb().commit_tree(&head).expect("commit tree");
    let mut index = repo.index_handle().load().expect("index");
    // Force tree↔worktree diff (not index↔worktree fast path) so legacy tree modes are tested.
    index.cache_tree_root = None;
    index.cache_tree = None;
    let mut uncommitted = UncommittedDiff::new(&repo, index);
    let entries = uncommitted
        .worktree_entries(Some(&head_tree), RenameDetection::Off)
        .expect("worktree diff vs HEAD");
    assert!(
        entries.is_empty(),
        "grit diff (worktree vs HEAD) must be empty: {entries:?}"
    );
}

#[test]
fn grit_checkout_writes_canonical_index_modes() {
    let root = tempfile::tempdir().expect("tempdir");
    let tree = init_legacy_mode_repo(root.path());
    let repo = open_repo(root.path());

    checkout_between_trees(&repo, None, &tree).expect("checkout");

    let ls = git(root.path(), &["ls-files", "-s"]);
    assert!(
        ls.lines()
            .any(|l| l.starts_with("100644 ") && l.ends_with("\ta.txt")),
        "index must store canonical 100644 for a.txt: {ls}"
    );
    assert!(
        ls.lines()
            .any(|l| l.starts_with("100755 ") && l.ends_with("\tz.txt")),
        "index must store canonical 100755 for z.txt: {ls}"
    );
    assert!(
        !ls.contains("100664"),
        "non-canonical 100664 must not appear in index: {ls}"
    );

    assert_index_modes_canonical(root.path());

    let model = status(
        &repo,
        &StatusOptions::default(),
        &mut NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("status");
    assert!(model.is_clean(), "grit status must be clean");

    let obj = repo.odb().read(&tree).expect("read tree");
    assert_eq!(obj.kind, ObjectKind::Tree);
    let entries = parse_tree(&obj.data).expect("parse tree");
    assert!(
        entries
            .iter()
            .any(|e| e.name == b"a.txt" && e.mode == 0o100664),
        "tree object must still record legacy 100664"
    );
}

#[test]
fn hard_reset_to_head_keeps_canonical_index_modes() {
    let root = tempfile::tempdir().expect("tempdir");
    init_legacy_mode_repo(root.path());
    let repo = open_repo(root.path());

    let head = repo.head_commit().expect("head commit");
    reset_to_commit(
        &repo,
        &ResetOptions::new(
            head,
            ResetMode::Hard,
            IDENT.parse().expect("identity"),
            "reset: test",
        ),
        &mut NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("hard reset HEAD");

    assert_index_modes_canonical(root.path());

    let head_tree = repo.odb().commit_tree(&head).expect("commit tree");
    let mut uncommitted = Diff::new(&repo).uncommitted().expect("uncommitted diff");
    let entries = uncommitted
        .worktree_entries(Some(&head_tree), RenameDetection::Off)
        .expect("worktree diff");
    assert!(entries.is_empty(), "diff after hard reset must be empty");
}
