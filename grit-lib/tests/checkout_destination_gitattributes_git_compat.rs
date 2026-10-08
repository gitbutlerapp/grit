//! Checkout smudge uses `.gitattributes` from the destination tree, not the pre-checkout worktree.

use std::process::Command;

use grit_lib::objects::ObjectId;
use grit_lib::porcelain::checkout::checkout_between_trees;
use grit_lib::repo::Repository;

fn git(repo: &std::path::Path, args: &[&str]) {
    let status = Command::new("git")
        .current_dir(repo)
        .args(args)
        .status()
        .expect("git");
    assert!(status.success(), "git {:?} failed", args);
}

fn tree_of_head(repo: &std::path::Path) -> ObjectId {
    let out = Command::new("git")
        .current_dir(repo)
        .args(["rev-parse", "HEAD^{tree}"])
        .output()
        .expect("rev-parse");
    assert!(out.status.success());
    let hex = String::from_utf8_lossy(&out.stdout).trim().to_string();
    ObjectId::from_hex(&hex).expect("tree oid")
}

#[test]
fn checkout_applies_destination_gitattributes_eol_crlf() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main", "."]);
    git(root, &["config", "user.email", "t@example.com"]);
    git(root, &["config", "user.name", "Test"]);

    std::fs::write(root.join("f.txt"), b"main\n").expect("main file");
    git(root, &["add", "f.txt"]);
    git(root, &["commit", "-qm", "main"]);
    let main_tree = tree_of_head(root);

    git(root, &["checkout", "-q", "-b", "br"]);
    std::fs::write(root.join(".gitattributes"), b"*.txt text eol=crlf\n").expect("attrs");
    std::fs::write(root.join("f.txt"), b"branch\n").expect("branch file");
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "branch"]);
    let br_tree = tree_of_head(root);

    git(root, &["checkout", "-q", "main"]);

    let grit_repo = Repository::discover(Some(root)).expect("open");
    checkout_between_trees(&grit_repo, Some(&main_tree), &br_tree).expect("grit checkout");

    let grit_bytes = std::fs::read(root.join("f.txt")).expect("read grit checkout");
    assert_eq!(
        grit_bytes,
        b"branch\r\n",
        "destination .gitattributes must drive eol=crlf smudge"
    );

    git(root, &["checkout", "-q", "br"]);
    let git_bytes = std::fs::read(root.join("f.txt")).expect("read git checkout");
    assert_eq!(
        grit_bytes, git_bytes,
        "grit checkout bytes must match system git for the same tree"
    );
}
