//! Issue #2403: rebase and conflicted merge abort must not leave unreferenced empty blob/tree.

use std::path::Path;
use std::process::Command;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::PostOperationHooks;
use grit_lib::objects::{HashAlgo, ObjectId};
use grit_lib::porcelain::merge::{abort_branch_merge, integrate_branch, IntegrateBranchOutcome};
use grit_lib::progress::{NeverCancel, NullProgress};
use grit_lib::repo::Repository;
use grit_lib::revwalk::history::{
    start_rebase, ApplyOptions, EmptyPolicy, RebaseOntoOptions, RebaseOutcome, RebaseRequest,
    RefScope, RewriteOptions, StartRebaseOptions,
};
use grit_test_support::git;
use grit_test_support::objects::git_supports_sha256;

const EMPTY_BLOB_SHA1: &str = "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391";
const EMPTY_TREE_SHA1: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
const EMPTY_BLOB_SHA256: &str = "473a0f4c8025794925fb1a877869d6389cf612d";
const EMPTY_TREE_SHA256: &str = "6ef19b41225c5369f1c104d45d8d85efa9b057b53b14b4b9b939dd74decc5321";

fn object_loose(git_dir: &Path, hex: &str) -> std::path::PathBuf {
    git_dir.join(format!("objects/{}/{}", &hex[..2], &hex[2..]))
}

fn git_fsck_strict(worktree: &Path) {
    let out = Command::new("git")
        .current_dir(worktree)
        .args(["fsck", "--strict"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git fsck");
    assert!(
        out.status.success(),
        "git fsck --strict: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("dangling tree"),
        "unexpected dangling tree:\n{stderr}"
    );
    assert!(
        !stderr.contains("dangling blob"),
        "unexpected dangling blob:\n{stderr}"
    );
}

fn assert_canonical_empty_objects_absent(git_dir: &Path, blob_hex: &str, tree_hex: &str) {
    assert!(
        !object_loose(git_dir, blob_hex).exists(),
        "canonical empty blob must not be stored loose"
    );
    assert!(
        !object_loose(git_dir, tree_hex).exists(),
        "canonical empty tree must not be stored loose"
    );
}

fn open(root: &Path) -> Repository {
    Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open grit")
}

fn oid(root: &Path, rev: &str) -> ObjectId {
    ObjectId::from_hex(git(root, &["rev-parse", rev]).trim()).expect("oid")
}

fn rewrite_options() -> RewriteOptions {
    RewriteOptions {
        scope: RefScope::Head,
        empty: EmptyPolicy::Drop,
        committer: "A <a@x.test>".parse().expect("signature"),
        preserve_committer: false,
        sign: false,
    }
}

fn apply_options() -> ApplyOptions {
    ApplyOptions {
        identity: "A <a@x.test>".parse().expect("identity"),
        reflog_message: "rebase".into(),
        record_operation: None,
        hooks: PostOperationHooks::Skip,
    }
}

fn start_rebase_main_side(root: &Path) {
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "user.name", "A"]);
    git(root, &["config", "user.email", "a@x.test"]);

    std::fs::write(root.join("a.txt"), "a").expect("write a");
    git(root, &["add", "a.txt"]);
    git(root, &["commit", "-m", "base"]);
    git(root, &["branch", "side"]);
    std::fs::write(root.join("b.txt"), "b").expect("write b");
    git(root, &["add", "b.txt"]);
    git(root, &["commit", "-m", "main2"]);
    git(root, &["switch", "side"]);
    std::fs::write(root.join("c.txt"), "c").expect("write c");
    git(root, &["add", "c.txt"]);
    git(root, &["commit", "-m", "side1"]);
}

#[test]
fn clean_add_only_rebase_leaves_no_dangling_empty_objects() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    start_rebase_main_side(root);

    let git_dir = root.join(".git");
    git_fsck_strict(root);
    assert_canonical_empty_objects_absent(&git_dir, EMPTY_BLOB_SHA1, EMPTY_TREE_SHA1);

    let main = oid(root, "main");
    let side_tip = oid(root, "side");
    let repo = open(root);
    let outcome = start_rebase(
        &repo,
        &StartRebaseOptions {
            request: RebaseRequest {
                upstream: main,
                onto: None,
                branch_tip: side_tip,
            },
            rebase: RebaseOntoOptions::new(rewrite_options()),
            apply: apply_options(),
            pre_rebase: grit_lib::revwalk::history::PreRebase::Skip,
        },
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("rebase");
    assert!(matches!(outcome, RebaseOutcome::Done { .. }));

    assert_canonical_empty_objects_absent(&git_dir, EMPTY_BLOB_SHA1, EMPTY_TREE_SHA1);
    git_fsck_strict(root);
}

#[test]
fn conflicted_merge_abort_leaves_no_dangling_empty_objects() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "user.name", "A"]);
    git(root, &["config", "user.email", "a@x.test"]);

    std::fs::write(root.join("a.txt"), "a").expect("write a");
    git(root, &["add", "a.txt"]);
    git(root, &["commit", "-m", "base"]);
    git(root, &["branch", "side"]);
    std::fs::write(root.join("a.txt"), "m").expect("write m");
    git(root, &["add", "a.txt"]);
    git(root, &["commit", "-m", "m"]);
    git(root, &["switch", "side"]);
    std::fs::write(root.join("a.txt"), "s").expect("write s");
    git(root, &["add", "a.txt"]);
    git(root, &["commit", "-m", "s"]);

    let git_dir = root.join(".git");
    git_fsck_strict(root);

    let repo = open(root);
    let outcome = integrate_branch(
        &repo,
        "refs/heads/main",
        oid(root, "main"),
        oid(root, "side"),
        "main",
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("merge");
    assert!(matches!(outcome, IntegrateBranchOutcome::Conflicts { .. }));

    abort_branch_merge(&repo).expect("abort merge");
    assert_canonical_empty_objects_absent(&git_dir, EMPTY_BLOB_SHA1, EMPTY_TREE_SHA1);
    git_fsck_strict(root);
}

#[test]
fn conflict_with_clean_add_keeps_added_blob_without_extra_danglers() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "user.name", "A"]);
    git(root, &["config", "user.email", "a@x.test"]);

    std::fs::write(root.join("a.txt"), "base").expect("write a");
    git(root, &["add", "a.txt"]);
    git(root, &["commit", "-m", "base"]);
    git(root, &["branch", "side"]);
    git(root, &["switch", "side"]);
    std::fs::write(root.join("added.txt"), "new on side").expect("write added");
    git(root, &["add", "added.txt"]);
    git(root, &["commit", "-m", "side add"]);
    git(root, &["switch", "main"]);
    std::fs::write(root.join("a.txt"), "main change").expect("write a");
    git(root, &["add", "a.txt"]);
    git(root, &["commit", "-m", "main change"]);

    let git_dir = root.join(".git");
    let added_blob = git(root, &["rev-parse", "side:added.txt"])
        .trim()
        .to_owned();

    let repo = open(root);
    let outcome = integrate_branch(
        &repo,
        "refs/heads/main",
        oid(root, "main"),
        oid(root, "side"),
        "side",
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("merge");
    assert!(matches!(outcome, IntegrateBranchOutcome::Conflicts { .. }));

    assert!(
        object_loose(&git_dir, &added_blob).exists(),
        "clean-add blob from the other branch must remain reachable"
    );
    abort_branch_merge(&repo).expect("abort");
    assert!(
        object_loose(&git_dir, &added_blob).exists(),
        "added blob must stay after abort"
    );
    assert_canonical_empty_objects_absent(&git_dir, EMPTY_BLOB_SHA1, EMPTY_TREE_SHA1);
    git_fsck_strict(root);
}

#[test]
fn sha256_rebase_and_merge_abort_leave_no_dangling_empty_objects() {
    if !git_supports_sha256() {
        eprintln!("SKIP: system git lacks --object-format=sha256");
        return;
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git(
        root,
        &["init", "-q", "--object-format=sha256", "-b", "main"],
    );
    git(root, &["config", "user.name", "A"]);
    git(root, &["config", "user.email", "a@x.test"]);

    std::fs::write(root.join("a.txt"), "a").expect("write a");
    git(root, &["add", "a.txt"]);
    git(root, &["commit", "-m", "base"]);
    git(root, &["branch", "side"]);
    std::fs::write(root.join("b.txt"), "b").expect("write b");
    git(root, &["add", "b.txt"]);
    git(root, &["commit", "-m", "main2"]);
    git(root, &["switch", "side"]);
    std::fs::write(root.join("c.txt"), "c").expect("write c");
    git(root, &["add", "c.txt"]);
    git(root, &["commit", "-m", "side1"]);

    let git_dir = root.join(".git");
    let repo = open(root);
    assert_eq!(repo.odb().hash_algo(), HashAlgo::Sha256);

    let main = oid(root, "main");
    let side_tip = oid(root, "side");
    let outcome = start_rebase(
        &repo,
        &StartRebaseOptions {
            request: RebaseRequest {
                upstream: main,
                onto: None,
                branch_tip: side_tip,
            },
            rebase: RebaseOntoOptions::new(rewrite_options()),
            apply: apply_options(),
            pre_rebase: grit_lib::revwalk::history::PreRebase::Skip,
        },
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("rebase");
    assert!(matches!(outcome, RebaseOutcome::Done { .. }));
    assert_canonical_empty_objects_absent(&git_dir, EMPTY_BLOB_SHA256, EMPTY_TREE_SHA256);
    git_fsck_strict(root);

    git(root, &["switch", "main"]);
    std::fs::write(root.join("a.txt"), "m").expect("write m");
    git(root, &["add", "a.txt"]);
    git(root, &["commit", "-m", "m2"]);
    git(root, &["switch", "side"]);
    std::fs::write(root.join("a.txt"), "s").expect("write s");
    git(root, &["add", "a.txt"]);
    git(root, &["commit", "-m", "s2"]);

    let outcome = integrate_branch(
        &repo,
        "refs/heads/main",
        oid(root, "main"),
        oid(root, "side"),
        "main",
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("merge");
    assert!(matches!(outcome, IntegrateBranchOutcome::Conflicts { .. }));
    abort_branch_merge(&repo).expect("abort");
    assert_canonical_empty_objects_absent(&git_dir, EMPTY_BLOB_SHA256, EMPTY_TREE_SHA256);
    git_fsck_strict(root);
}
