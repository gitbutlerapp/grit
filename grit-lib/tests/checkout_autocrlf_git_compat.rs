//! Checkout applies `core.autocrlf` smudge (LF→CRLF) like Git checkout.

use std::fs;
use std::process::Command;

use grit_lib::objects::{serialize_commit, CommitData, ObjectKind};
use grit_lib::porcelain::add::{stage, StageOptions};
use grit_lib::porcelain::checkout::checkout_between_trees;
use grit_lib::progress::NullProgress;
use grit_lib::refs;
use grit_lib::repo::{init_repository, Repository};
use grit_lib::write_tree::{write_tree_update_index, WriteTreeFlags};

fn git_eol(repo: &std::path::Path, path: &str) -> String {
    let out = Command::new("git")
        .current_dir(repo)
        .args(["ls-files", "--eol", path])
        .output()
        .expect("git ls-files --eol");
    assert!(
        out.status.success(),
        "git ls-files --eol failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn init_autocrlf_repo(root: &std::path::Path) -> Repository {
    let repo = init_repository(root, false, "main", None, "files").expect("init");
    fs::write(
        root.join(".git/config"),
        "[core]\n\trepositoryformatversion = 0\n\tbare = false\n\tautocrlf = true\n",
    )
    .expect("config");
    repo.reload_config().expect("reload config");
    repo
}

fn commit_all(repo: &Repository, message: &str) -> grit_lib::objects::ObjectId {
    stage(repo, &StageOptions::default(), &mut NullProgress).expect("stage");
    let mut index = repo.load_index().expect("index");
    let tree =
        write_tree_update_index(&repo.odb, &mut index, "", WriteTreeFlags::silent()).expect("tree");
    repo.write_index(&mut index).expect("write index");
    let parent = refs::resolve_ref(&repo.git_dir, "HEAD").ok();
    let commit_data = CommitData {
        tree,
        parents: parent.into_iter().collect(),
        author: "Test <t@example.com>".to_owned(),
        committer: "Test <t@example.com>".to_owned(),
        author_raw: Vec::new(),
        committer_raw: Vec::new(),
        encoding: None,
        message: format!("{message}\n"),
        raw_message: None,
        extra_headers: Vec::new(),
    };
    let bytes = serialize_commit(&commit_data);
    let commit_oid = repo.odb.write(ObjectKind::Commit, &bytes).expect("commit");
    refs::write_ref(&repo.git_dir, "HEAD", &commit_oid).expect("head");
    tree
}

#[test]
fn checkout_autocrlf_writes_crlf_to_worktree() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = init_autocrlf_repo(tmp.path());
    let wt = repo.work_tree.as_ref().expect("worktree");

    fs::write(wt.join("f.txt"), b"a\r\nb\r\nc\r\n").expect("write");
    let main_tree = commit_all(&repo, "base");

    fs::write(wt.join("f.txt"), b"A\r\nB\r\nC\r\n").expect("branch write");
    let branch_tree = commit_all(&repo, "branch");

    checkout_between_trees(&repo, Some(&branch_tree), &main_tree).expect("switch to main");

    let on_disk = fs::read(wt.join("f.txt")).expect("read worktree");
    assert_eq!(
        on_disk, b"a\r\nb\r\nc\r\n",
        "checkout must smudge LF index blobs to CRLF when core.autocrlf=true"
    );

    Command::new("git")
        .current_dir(tmp.path())
        .args(["config", "user.email", "t@example.com"])
        .status()
        .expect("git config email");
    Command::new("git")
        .current_dir(tmp.path())
        .args(["config", "user.name", "Test"])
        .status()
        .expect("git config name");

    let eol = git_eol(tmp.path(), "f.txt");
    assert!(
        eol.contains("w/crlf"),
        "git ls-files --eol should report worktree CRLF, got: {eol:?}"
    );
}
