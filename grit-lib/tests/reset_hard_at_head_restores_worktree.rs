//! Hard reset at HEAD must restore the worktree when tracked files were touched without
//! moving HEAD.

use crate::support;

use grit_lib::index::write_tree::{write_tree_update_index, WriteTreeFlags};
use grit_lib::objects::{serialize_commit, CommitData, ObjectKind};
use grit_lib::porcelain::reset::{reset_to_commit, ResetMode, ResetOptions};
use grit_lib::refs;
use grit_lib::repo::{InitOptions, Repository};
use grit_test_support::git;
use tempfile::TempDir;

fn init_repo() -> (TempDir, Repository) {
    let dir = TempDir::new().expect("tempdir");
    let repo = Repository::init(dir.path(), &InitOptions::default()).expect("init");
    (dir, repo)
}

fn commit_file(repo: &Repository, path: &str, contents: &str) -> grit_lib::objects::ObjectId {
    let work_tree = repo.work_tree().expect("wt");
    std::fs::write(work_tree.join(path), contents).expect("write");
    let mut index = repo.index_handle().load().expect("index");
    let oid = repo
        .odb()
        .write(ObjectKind::Blob, contents.as_bytes())
        .expect("blob");
    let entry = grit_lib::index::entry_from_stat(
        &work_tree.join(path),
        path.as_bytes(),
        oid,
        grit_lib::index::MODE_REGULAR,
    )
    .expect("entry");
    index.add_or_replace(entry);
    index.sort();
    let tree = write_tree_update_index(repo.odb(), &mut index, "", WriteTreeFlags::silent())
        .expect("tree");
    repo.index_handle().write(&mut index).expect("write index");
    let commit = CommitData {
        tree,
        parents: vec![],
        author: "T <t@t> 0 +0000".into(),
        committer: "T <t@t> 0 +0000".into(),
        author_raw: vec![],
        committer_raw: vec![],
        encoding: None,
        message: "c\n".into(),
        raw_message: None,
        extra_headers: Vec::new(),
    };
    let co = repo
        .odb()
        .write(ObjectKind::Commit, &serialize_commit(&commit))
        .expect("commit");
    refs::write_ref(repo.git_dir(), "refs/heads/main", &co).expect("branch");
    refs::write_symbolic_ref(repo.git_dir(), "HEAD", "refs/heads/main").expect("symhead");
    co
}

fn reset_opts(target: grit_lib::objects::ObjectId, mode: ResetMode) -> ResetOptions {
    ResetOptions::new(
        target,
        mode,
        support::parse_reflog_signature("T <t@t> 0 +0000"),
        "reset",
    )
}

fn hard_reset_at_head(repo: &Repository, head: grit_lib::objects::ObjectId) {
    reset_to_commit(
        repo,
        &reset_opts(head, ResetMode::Hard),
        &mut grit_lib::progress::NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("hard reset at HEAD");
}

#[test]
fn hard_reset_at_head_restores_same_size_mtime_content_edit() {
    let (dir, repo) = init_repo();
    let head = commit_file(&repo, "tracked.txt", "hello world\n");
    let path = repo.work_tree().expect("wt").join("tracked.txt");
    let pinned =
        filetime::FileTime::from_last_modification_time(&std::fs::metadata(&path).expect("stat"));
    std::fs::write(&path, "Xello world\n").expect("same-size edit");
    filetime::set_file_mtime(&path, pinned).expect("restore mtime");

    hard_reset_at_head(&repo, head);

    assert_eq!(
        std::fs::read(&path).expect("read"),
        b"hello world\n",
        "reset --hard at HEAD must restore blob bytes when stat still matches"
    );
    let porcelain = git(dir.path(), &["status", "--porcelain"]);
    assert!(
        porcelain.trim().is_empty(),
        "repo must be clean: {porcelain}"
    );
}

#[test]
fn hard_reset_at_head_restores_racy_index_second_edit() {
    let (dir, repo) = init_repo();
    let head = commit_file(&repo, "racy.txt", "base line\n");
    let path = repo.work_tree().expect("wt").join("racy.txt");
    let pinned = filetime::FileTime::from_unix_time(1_700_000_000, 0);
    filetime::set_file_mtime(&path, pinned).expect("pin worktree");
    let index_path = dir.path().join(".git/index");
    filetime::set_file_mtime(&index_path, pinned).expect("pin index mtime");

    std::fs::write(&path, "Xase line\n").expect("same-size edit");
    filetime::set_file_mtime(&path, pinned).expect("forge mtime");

    hard_reset_at_head(&repo, head);

    assert_eq!(std::fs::read(&path).expect("read"), b"base line\n");
    let porcelain = git(dir.path(), &["status", "--porcelain"]);
    assert!(
        porcelain.trim().is_empty(),
        "repo must be clean: {porcelain}"
    );
}

#[cfg(unix)]
#[test]
fn hard_reset_at_head_restores_chmod_only_change() {
    use std::os::unix::fs::PermissionsExt;

    let (dir, repo) = init_repo();
    let wt = repo.work_tree().expect("wt");
    let path = wt.join("exec.sh");
    std::fs::write(&path, "#!/bin/sh\n").expect("write");
    let mut perms = std::fs::metadata(&path).expect("meta").permissions();
    perms.set_mode(0o100755);
    std::fs::set_permissions(&path, perms).expect("chmod exec");
    let head = {
        let mut index = repo.index_handle().load().expect("index");
        let oid = repo
            .odb()
            .write(ObjectKind::Blob, b"#!/bin/sh\n")
            .expect("blob");
        let entry = grit_lib::index::entry_from_stat(
            &path,
            b"exec.sh",
            oid,
            grit_lib::index::MODE_EXECUTABLE,
        )
        .expect("entry");
        index.add_or_replace(entry);
        index.sort();
        let tree = write_tree_update_index(repo.odb(), &mut index, "", WriteTreeFlags::silent())
            .expect("tree");
        repo.index_handle().write(&mut index).expect("write index");
        let commit = CommitData {
            tree,
            parents: vec![],
            author: "T <t@t> 0 +0000".into(),
            committer: "T <t@t> 0 +0000".into(),
            author_raw: vec![],
            committer_raw: vec![],
            encoding: None,
            message: "c\n".into(),
            raw_message: None,
            extra_headers: Vec::new(),
        };
        repo.odb()
            .write(ObjectKind::Commit, &serialize_commit(&commit))
            .expect("commit")
    };
    refs::write_ref(repo.git_dir(), "refs/heads/main", &head).expect("branch");
    refs::write_symbolic_ref(repo.git_dir(), "HEAD", "refs/heads/main").expect("symhead");
    let mut perms = std::fs::metadata(&path).expect("meta").permissions();
    perms.set_mode(0o100644);
    std::fs::set_permissions(&path, perms).expect("chmod");

    hard_reset_at_head(&repo, head);

    let mode = std::fs::metadata(&path).expect("meta").permissions().mode() & 0o777;
    assert_eq!(mode, 0o755, "reset --hard must restore the executable bit");
    let porcelain = git(dir.path(), &["status", "--porcelain"]);
    assert!(
        porcelain.trim().is_empty(),
        "repo must be clean: {porcelain}"
    );
}
