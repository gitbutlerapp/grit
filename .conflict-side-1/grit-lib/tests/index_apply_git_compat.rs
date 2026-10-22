//! Index-level patch apply vs `git apply --cached`.

use std::process::Command;

use grit_lib::apply::{
    apply_patch_to_index, apply_patch_to_index_and_write, parse_patch, IndexApplyOptions,
};
use grit_lib::config::environment::RepositoryOptions;
use grit_lib::index::write_tree::write_tree_from_index;
use grit_lib::objects::ObjectId;
use grit_lib::porcelain::status::{status, StatusOptions};
use grit_lib::progress::NullProgress;
use grit_lib::repo::Repository;
use tempfile::TempDir;

fn git(dir: &std::path::Path, args: &[&str]) {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@e.com")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@e.com")
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn git_tree(dir: &std::path::Path) -> ObjectId {
    let out = Command::new("git")
        .current_dir(dir)
        .args(["write-tree"])
        .output()
        .expect("write-tree");
    assert!(out.status.success());
    ObjectId::from_hex(String::from_utf8_lossy(&out.stdout).trim()).expect("tree oid")
}

fn grit_tree(repo: &Repository, index: &grit_lib::index::Index) -> ObjectId {
    write_tree_from_index(repo.odb(), index, "").expect("write-tree")
}

fn parse(text: &str) -> Vec<grit_lib::apply::FilePatch> {
    parse_patch(text, 1, "patch", false, None).expect("parse")
}

#[test]
fn index_apply_create_delete() {
    let dir = TempDir::new().expect("tempdir");
    git(dir.path(), &["init", "-q", "-b", "main"]);
    std::fs::write(dir.path().join("keep.txt"), b"k\n").unwrap();
    git(dir.path(), &["add", "keep.txt"]);
    git(dir.path(), &["commit", "-qm", "init"]);

    let add_patch = parse(
        "diff --git a/new.txt b/new.txt\n\
         new file mode 100644\n\
         --- /dev/null\n\
         +++ b/new.txt\n\
         @@ -0,0 +1 @@\n\
         +hello\n",
    );
    let repo = Repository::discover(Some(dir.path()), &RepositoryOptions::default()).unwrap();
    let mut index = repo.index_handle().load().unwrap();
    apply_patch_to_index(&repo, &mut index, &add_patch, &IndexApplyOptions::default()).unwrap();
    assert!(index.get(b"new.txt", 0).is_some());

    let del_patch = parse(
        "diff --git a/keep.txt b/keep.txt\n\
         deleted file mode 100644\n\
         --- a/keep.txt\n\
         +++ /dev/null\n\
         @@ -1 +0,0 @@\n\
         -k\n",
    );
    apply_patch_to_index(&repo, &mut index, &del_patch, &IndexApplyOptions::default()).unwrap();
    assert!(index.get(b"keep.txt", 0).is_none());
}

#[test]
fn index_apply_mode_change() {
    let dir = TempDir::new().expect("tempdir");
    git(dir.path(), &["init", "-q", "-b", "main"]);
    std::fs::write(dir.path().join("run.sh"), b"#!/bin/sh\n").unwrap();
    git(dir.path(), &["add", "run.sh"]);
    git(dir.path(), &["commit", "-qm", "init"]);

    let patch = parse(
        "diff --git a/run.sh b/run.sh\n\
         old mode 100644\n\
         new mode 100755\n\
         --- a/run.sh\n\
         +++ b/run.sh\n",
    );
    let repo = Repository::discover(Some(dir.path()), &RepositoryOptions::default()).unwrap();
    let mut index = repo.index_handle().load().unwrap();
    apply_patch_to_index(&repo, &mut index, &patch, &IndexApplyOptions::default()).unwrap();
    assert_eq!(index.get(b"run.sh", 0).unwrap().mode, 0o100755);
}

#[test]
fn index_apply_rename() {
    let dir = TempDir::new().expect("tempdir");
    git(dir.path(), &["init", "-q", "-b", "main"]);
    std::fs::write(dir.path().join("old.txt"), b"x\n").unwrap();
    git(dir.path(), &["add", "old.txt"]);
    git(dir.path(), &["commit", "-qm", "init"]);

    let patch = parse(
        "diff --git a/old.txt b/new.txt\n\
         similarity index 100%\n\
         rename from old.txt\n\
         rename to new.txt\n",
    );
    let repo = Repository::discover(Some(dir.path()), &RepositoryOptions::default()).unwrap();
    let mut index = repo.index_handle().load().unwrap();
    apply_patch_to_index(&repo, &mut index, &patch, &IndexApplyOptions::default()).unwrap();
    assert!(index.get(b"old.txt", 0).is_none());
    assert!(index.get(b"new.txt", 0).is_some());
}

#[test]
fn index_apply_binary() {
    let dir = TempDir::new().expect("tempdir");
    git(dir.path(), &["init", "-q", "-b", "main"]);
    let old = b"\0BIN\0";
    let new = b"\0BIN2\0";
    std::fs::write(dir.path().join("b.bin"), old).unwrap();
    git(dir.path(), &["add", "b.bin"]);
    git(dir.path(), &["commit", "-qm", "init"]);

    let body = grit_lib::apply::write_git_binary_patch(old, new, "a/b.bin", "b/b.bin").unwrap();
    let patch_text = format!("diff --git a/b.bin b/b.bin\n{body}");
    let patch = parse(&patch_text);

    let repo = Repository::discover(Some(dir.path()), &RepositoryOptions::default()).unwrap();
    let mut index = repo.index_handle().load().unwrap();
    apply_patch_to_index(&repo, &mut index, &patch, &IndexApplyOptions::default()).unwrap();
    let oid = index.get(b"b.bin", 0).unwrap().oid;
    assert_eq!(repo.odb().read(&oid).unwrap().data, new);
}

#[test]
fn index_apply_atomic_on_failure() {
    let dir = TempDir::new().expect("tempdir");
    git(dir.path(), &["init", "-q", "-b", "main"]);
    std::fs::write(dir.path().join("a.txt"), b"one\n").unwrap();
    git(dir.path(), &["add", "a.txt"]);
    git(dir.path(), &["commit", "-qm", "init"]);

    let good = parse(
        "diff --git a/a.txt b/a.txt\n\
         --- a/a.txt\n\
         +++ b/a.txt\n\
         @@ -1 +1 @@\n\
         -one\n\
         +two\n",
    );
    let bad = parse(
        "diff --git a/missing.txt b/missing.txt\n\
         --- a/missing.txt\n\
         +++ b/missing.txt\n\
         @@ -1 +1 @@\n\
         -nope\n\
         +nope2\n",
    );
    let repo = Repository::discover(Some(dir.path()), &RepositoryOptions::default()).unwrap();
    let mut index = repo.index_handle().load().unwrap();
    let before = index.get(b"a.txt", 0).unwrap().oid;
    let mut batch = good.clone();
    batch.extend(bad);
    let err = apply_patch_to_index(&repo, &mut index, &batch, &IndexApplyOptions::default())
        .expect_err("should fail");
    assert!(matches!(
        err,
        grit_lib::error::ApplyError::NotInIndex { .. }
    ));
    assert_eq!(index.get(b"a.txt", 0).unwrap().oid, before);
}

#[test]
fn index_apply_keeps_worktree_dirty_in_status() {
    let dir = TempDir::new().expect("tempdir");
    git(dir.path(), &["init", "-q", "-b", "main"]);
    std::fs::write(dir.path().join("f.txt"), b"worktree\n").unwrap();
    git(dir.path(), &["add", "f.txt"]);
    git(dir.path(), &["commit", "-qm", "init"]);

    let patch = parse(
        "diff --git a/f.txt b/f.txt\n\
         --- a/f.txt\n\
         +++ b/f.txt\n\
         @@ -1 +1 @@\n\
         -worktree\n\
         +index\n",
    );
    let repo = Repository::discover(Some(dir.path()), &RepositoryOptions::default()).unwrap();
    apply_patch_to_index_and_write(&repo, &patch, &IndexApplyOptions::default()).unwrap();

    let mut progress = NullProgress;
    let st = status(&repo, &StatusOptions::default(), &mut progress).expect("status");
    assert!(
        st.staged.iter().any(|e| e.path() == "f.txt"),
        "expected staged index change: {:?}",
        st.staged
    );
    assert!(
        st.unstaged.iter().any(|e| e.path() == "f.txt"),
        "expected worktree differs from index: {:?}",
        st.unstaged
    );
}

#[test]
fn index_apply_no_newline_preimage() {
    let dir = TempDir::new().expect("tempdir");
    git(dir.path(), &["init", "-q", "-b", "main"]);
    std::fs::write(dir.path().join("f.txt"), b"old").unwrap();
    git(dir.path(), &["add", "f.txt"]);
    git(dir.path(), &["commit", "-qm", "init"]);
    std::fs::write(dir.path().join("f.txt"), b"new\n").unwrap();
    let patch = String::from_utf8_lossy(
        &Command::new("git")
            .current_dir(dir.path())
            .args(["diff"])
            .output()
            .expect("diff")
            .stdout,
    )
    .to_string();
    assert!(patch.contains("No newline"), "{patch}");
    let repo = Repository::discover(Some(dir.path()), &RepositoryOptions::default()).unwrap();
    let patches = parse(&patch);
    apply_patch_to_index_and_write(&repo, &patches, &IndexApplyOptions::default()).unwrap();
    let entry = repo.index_handle().load().unwrap();
    let e = entry.get(b"f.txt", 0).expect("indexed");
    assert_eq!(repo.odb().read(&e.oid).unwrap().data, b"new\n");
}

#[test]
fn index_apply_deletion_rejects_stale_preimage() {
    let dir = TempDir::new().expect("tempdir");
    git(dir.path(), &["init", "-q", "-b", "main"]);
    std::fs::write(dir.path().join("f.txt"), b"actual\n").unwrap();
    git(dir.path(), &["add", "f.txt"]);
    git(dir.path(), &["commit", "-qm", "init"]);
    let patch = parse(
        "diff --git a/f.txt b/f.txt\n\
         deleted file mode 100644\n\
         --- a/f.txt\n\
         +++ /dev/null\n\
         @@ -1 +0,0 @@\n\
         -expected\n",
    );
    let repo = Repository::discover(Some(dir.path()), &RepositoryOptions::default()).unwrap();
    let mut index = repo.index_handle().load().unwrap();
    let before = index.get(b"f.txt", 0).is_some();
    assert!(before);
    let err = apply_patch_to_index(&repo, &mut index, &patch, &IndexApplyOptions::default())
        .expect_err("must reject");
    assert!(matches!(
        err,
        grit_lib::error::ApplyError::PatchFailed { .. }
    ));
    assert!(index.get(b"f.txt", 0).is_some());
}

#[test]
fn index_apply_sha256_write_tree_roundtrip() {
    if !grit_test_support::objects::git_supports_sha256() {
        return;
    }
    let dir = TempDir::new().expect("tempdir");
    Command::new("git")
        .current_dir(dir.path())
        .args(["init", "-q", "-b", "main", "--object-format=sha256"])
        .status()
        .expect("init sha256");
    std::fs::write(dir.path().join("f.txt"), b"one\n").unwrap();
    git(dir.path(), &["add", "f.txt"]);
    git(dir.path(), &["commit", "-qm", "init"]);
    std::fs::write(dir.path().join("f.txt"), b"two\n").unwrap();
    let patch = String::from_utf8_lossy(
        &Command::new("git")
            .current_dir(dir.path())
            .args(["diff"])
            .output()
            .expect("diff")
            .stdout,
    )
    .to_string();
    let repo = Repository::discover(Some(dir.path()), &RepositoryOptions::default()).unwrap();
    assert_eq!(repo.odb().hash_algo(), grit_lib::objects::HashAlgo::Sha256);
    apply_patch_to_index_and_write(&repo, &parse(&patch), &IndexApplyOptions::default()).unwrap();
    let grit_oid = grit_tree(&repo, &repo.index_handle().load().unwrap());
    git(dir.path(), &["reset", "--hard", "HEAD"]);
    std::fs::write(dir.path().join("patch.diff"), &patch).unwrap();
    git(dir.path(), &["apply", "--cached", "patch.diff"]);
    let git_oid = git_tree(dir.path());
    assert_eq!(grit_oid, git_oid);
    git(dir.path(), &["write-tree"]);
    git(dir.path(), &["fsck", "--strict"]);
}

#[test]
fn index_apply_git_diff_roundtrip_write_tree() {
    let dir = TempDir::new().expect("tempdir");
    git(dir.path(), &["init", "-q", "-b", "main"]);
    std::fs::write(dir.path().join("plain.txt"), b"one\n").unwrap();
    git(dir.path(), &["add", "plain.txt"]);
    git(dir.path(), &["commit", "-qm", "init"]);
    std::fs::write(dir.path().join("plain.txt"), b"one\ntwo\n").unwrap();
    git(dir.path(), &["add", "plain.txt"]);
    git(dir.path(), &["commit", "-qm", "two"]);

    git(dir.path(), &["reset", "--hard", "HEAD~1"]);
    let patch_out = Command::new("git")
        .current_dir(dir.path())
        .args(["diff", "--binary", "HEAD", "HEAD@{1}"])
        .output()
        .expect("git diff");
    let patch = String::from_utf8_lossy(&patch_out.stdout).to_string();

    let repo = Repository::discover(Some(dir.path()), &RepositoryOptions::default()).unwrap();
    let patches = parse(&patch);
    apply_patch_to_index_and_write(&repo, &patches, &IndexApplyOptions::default()).unwrap();
    let grit_oid = grit_tree(&repo, &repo.index_handle().load().unwrap());

    git(dir.path(), &["reset", "--hard", "HEAD"]);
    std::fs::write(dir.path().join("patch.diff"), &patch).unwrap();
    git(dir.path(), &["apply", "--cached", "patch.diff"]);
    let git_oid = git_tree(dir.path());
    assert_eq!(grit_oid, git_oid);

    git(dir.path(), &["fsck", "--strict"]);
    let porcelain = Command::new("git")
        .current_dir(dir.path())
        .args(["status", "--porcelain"])
        .output()
        .expect("status");
    assert!(porcelain.status.success());
    let mut progress = NullProgress;
    let grit_status = status(&repo, &StatusOptions::default(), &mut progress).unwrap();
    let git_p = String::from_utf8_lossy(&porcelain.stdout);
    assert_eq!(
        grit_status.staged.len() + grit_status.unstaged.len(),
        git_p.lines().filter(|l| !l.is_empty()).count(),
        "git porcelain:\n{git_p}\ngrit staged {:?} unstaged {:?}",
        grit_status.staged,
        grit_status.unstaged
    );
}
