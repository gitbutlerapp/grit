//! Issue #1614: merge must not write an unreferenced canonical empty tree.

use std::path::Path;
use std::process::Command;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::{HashAlgo, ObjectId};
use grit_lib::porcelain::merge::{integrate_branch, IntegrateBranchOutcome};
use grit_lib::repo::Repository;
use grit_test_support::{git, git_supports_sha256};

const EMPTY_TREE_SHA1: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
const EMPTY_TREE_SHA256: &str = "6ef19b41225c5369f1c104d45d8d85efa9b057b53b14b4b9b939dd74decc5321";

fn empty_tree_loose(git_dir: &Path, hex: &str) -> std::path::PathBuf {
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
        "unexpected dangling tree in fsck output:\n{stderr}"
    );
}

#[test]
fn merge_diverged_branches_leaves_no_dangling_empty_tree() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "user.name", "a"]);
    git(root, &["config", "user.email", "a@x"]);

    std::fs::write(root.join("a"), "1\n").expect("write a");
    git(root, &["add", "a"]);
    git(root, &["commit", "-m", "base"]);
    git(root, &["branch", "side"]);
    std::fs::write(root.join("b"), "2\n").expect("write b");
    git(root, &["add", "b"]);
    git(root, &["commit", "-m", "main2"]);
    git(root, &["switch", "side"]);
    std::fs::write(root.join("c"), "3\n").expect("write c");
    git(root, &["add", "c"]);
    git(root, &["commit", "-m", "side"]);
    git(root, &["switch", "main"]);

    let git_dir = root.join(".git");
    git_fsck_strict(root);
    assert!(
        !empty_tree_loose(&git_dir, EMPTY_TREE_SHA1).exists(),
        "empty tree should not exist before merge"
    );

    let side: ObjectId = git(root, &["rev-parse", "side"])
        .trim()
        .parse()
        .expect("side oid");
    let main: ObjectId = git(root, &["rev-parse", "main"])
        .trim()
        .parse()
        .expect("main oid");

    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open grit");
    let outcome = integrate_branch(&repo, "refs/heads/main", main, side, "side").expect("merge");
    assert!(matches!(outcome, IntegrateBranchOutcome::Merged { .. }));

    assert!(
        !empty_tree_loose(&git_dir, EMPTY_TREE_SHA1).exists(),
        "merge must not materialize unreferenced empty tree"
    );
    git_fsck_strict(root);
}

/// One side adds a directory on a diverged branch; merge-tree walk uses the empty-tree
/// sentinel for the absent subtree (regression for SHA-256 virtual read).
#[test]
fn sha256_merge_directory_add_without_materializing_empty_tree() {
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
    git(root, &["config", "user.name", "a"]);
    git(root, &["config", "user.email", "a@x"]);

    std::fs::write(root.join("base.txt"), "base\n").expect("write base");
    git(root, &["add", "base.txt"]);
    git(root, &["commit", "-m", "base"]);
    git(root, &["branch", "side"]);
    git(root, &["switch", "side"]);
    std::fs::create_dir_all(root.join("added")).expect("mkdir");
    std::fs::write(root.join("added/leaf.txt"), "side\n").expect("write leaf");
    git(root, &["add", "added/leaf.txt"]);
    git(root, &["commit", "-m", "side adds directory"]);
    git(root, &["switch", "main"]);
    std::fs::write(root.join("main.txt"), "main\n").expect("write main");
    git(root, &["add", "main.txt"]);
    git(root, &["commit", "-m", "main diverged"]);

    let git_dir = root.join(".git");
    assert!(
        !empty_tree_loose(&git_dir, EMPTY_TREE_SHA256).exists(),
        "SHA-256 empty tree should not exist before merge"
    );

    let side: ObjectId = git(root, &["rev-parse", "side"])
        .trim()
        .parse()
        .expect("side oid");
    let main: ObjectId = git(root, &["rev-parse", "main"])
        .trim()
        .parse()
        .expect("main oid");

    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open grit");
    assert_eq!(repo.odb().hash_algo(), HashAlgo::Sha256);

    let outcome = integrate_branch(&repo, "refs/heads/main", main, side, "side").expect("merge");
    assert!(matches!(outcome, IntegrateBranchOutcome::Merged { .. }));

    assert!(
        !empty_tree_loose(&git_dir, EMPTY_TREE_SHA256).exists(),
        "merge must not materialize unreferenced SHA-256 empty tree"
    );
    git_fsck_strict(root);
}
