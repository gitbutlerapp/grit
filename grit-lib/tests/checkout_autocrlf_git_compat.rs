//! Checkout applies `core.autocrlf` smudge (LF→CRLF) like Git checkout.

use grit_lib::progress::NullProgress;
use std::fs;
use std::process::Command;

use grit_lib::index::write_tree::{write_tree_update_index, WriteTreeFlags};
use grit_lib::objects::{serialize_commit, CommitData, ObjectKind};
use grit_lib::porcelain::add::StageOptions;
use grit_lib::porcelain::checkout::checkout_between_trees;
use grit_lib::progress::NullProgress;
use grit_lib::repo::{InitOptions, Repository};

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
    let (repo, _) = Repository::init(
        root,
        &InitOptions::new()
            .with_initial_branch("main".to_owned())
            .with_ref_storage(grit_lib::refs::ref_storage::RefStorageFormat::Files),
    )
    .expect("init");
    fs::write(
        root.join(".git/config"),
        "[core]\n\trepositoryformatversion = 0\n\tbare = false\n\tautocrlf = true\n",
    )
    .expect("config");
    repo.reload_config().expect("reload config");
    repo
}

fn commit_all(repo: &Repository, message: &str) -> grit_lib::objects::ObjectId {
    repo.index()
        .stage(
            &StageOptions::default(),
            &mut NullProgress,
            &grit_lib::progress::CancelToken::never(),
        )
        .expect("stage");
    let mut index = repo
        .index()
        .load(&grit_lib::index::IndexLoadOptions::default())
        .expect("index");
    let tree = write_tree_update_index(repo.odb(), &mut index, "", WriteTreeFlags::silent())
        .expect("tree");
    repo.index()
        .write(&mut index, &grit_lib::index::IndexWriteOptions::default())
        .expect("write index");
    let parent = repo
        .references()
        .resolve(grit_lib::refs::store::RefNameRef::new("HEAD").expect("valid ref name"))
        .ok();
    let commit_data = CommitData::new(
        tree,
        parent.into_iter().collect(),
        "Test <t@example.com>",
        "Test <t@example.com>",
        format!("{message}\n"),
    );
    let bytes = serialize_commit(&commit_data);
    let commit_oid = repo
        .odb()
        .write(ObjectKind::Commit, &bytes)
        .expect("commit");
    grit_test_support::refs::set_ref(&repo, "HEAD", &commit_oid).expect("head");
    tree
}

#[test]
fn checkout_autocrlf_writes_crlf_to_worktree() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = init_autocrlf_repo(tmp.path());
    let wt = repo.work_tree().expect("worktree");

    fs::write(wt.join("f.txt"), b"a\r\nb\r\nc\r\n").expect("write");
    let main_tree = commit_all(&repo, "base");

    fs::write(wt.join("f.txt"), b"A\r\nB\r\nC\r\n").expect("branch write");
    let branch_tree = commit_all(&repo, "branch");

    repo.checkout()
        .checkout(
            Some(&branch_tree),
            &main_tree,
            &mut grit_lib::progress::NullProgress,
            &grit_lib::progress::CancelToken::never(),
        )
        .expect("switch to main");

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
