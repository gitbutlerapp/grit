//! Index/worktree diff when a gitlink path becomes a regular file (t4041-style).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use grit_lib::diff::{diff_index_to_worktree_with_options, DiffIndexToWorktreeOptions, DiffStatus};
use grit_lib::index::{Index, IndexEntry, MODE_GITLINK, MODE_REGULAR};
use grit_lib::objects::{ObjectId, ObjectKind};
use grit_lib::repo::{InitOptions, Repository};
use tempfile::tempdir;

fn gitlink_entry(path: &str, oid: ObjectId) -> IndexEntry {
    IndexEntry {
        ctime_sec: 0,
        ctime_nsec: 0,
        mtime_sec: 0,
        mtime_nsec: 0,
        dev: 0,
        ino: 0,
        mode: MODE_GITLINK,
        uid: 0,
        gid: 0,
        size: 0,
        oid,
        flags: path.len().min(0xfff) as u16,
        flags_extended: None,
        path: path.as_bytes().to_vec(),
        base_index_pos: 0,
    }
}

#[test]
fn gitlink_replaced_by_regular_file_reports_typechange() -> grit_lib::error::Result<()> {
    let dir = tempdir().map_err(grit_lib::error::Error::Io)?;
    let repo = Repository::init(dir.path(), &InitOptions::default())?;
    let submodule_tip = repo
        .odb()
        .write(ObjectKind::Commit, b"tree 0000000000000000000000000000000000000000\nauthor T <t@e.com> 1 +0000\ncommitter T <t@e.com> 1 +0000\n\nsub\n")?;
    let mut index = Index::new();
    index
        .entries_mut()
        .push(gitlink_entry("submod", submodule_tip));
    std::fs::write(dir.path().join("submod"), b"now a file\n")
        .map_err(grit_lib::error::Error::Io)?;

    let (diff, _) = diff_index_to_worktree_with_options(
        repo.odb(),
        &mut index,
        dir.path(),
        DiffIndexToWorktreeOptions {
            repository_git_dir: Some(repo.git_dir().to_path_buf()),
            simplify_gitlinks: true,
            ..DiffIndexToWorktreeOptions::default()
        },
    )?;
    assert!(
        diff.iter().any(|e| e.status == DiffStatus::TypeChanged && e.old_path.as_deref() == Some("submod")),
        "expected typechange for gitlink -> file, got {diff:?}"
    );

    let blob = repo.odb().write(ObjectKind::Blob, b"still file\n")?;
    index.entries_mut().clear();
    index.entries_mut().push(IndexEntry {
        mode: MODE_REGULAR,
        oid: blob,
        path: b"submod".to_vec(),
        ctime_sec: 0,
        ctime_nsec: 0,
        mtime_sec: 0,
        mtime_nsec: 0,
        dev: 0,
        ino: 0,
        uid: 0,
        gid: 0,
        size: 0,
        flags: 5,
        flags_extended: None,
        base_index_pos: 0,
    });
    std::fs::write(dir.path().join("submod"), b"still file\n")
        .map_err(grit_lib::error::Error::Io)?;
    let (clean, _) = diff_index_to_worktree_with_options(
        repo.odb(),
        &mut index,
        dir.path(),
        DiffIndexToWorktreeOptions {
            repository_git_dir: Some(repo.git_dir().to_path_buf()),
            ..DiffIndexToWorktreeOptions::default()
        },
    )?;
    assert!(clean.is_empty(), "matching file should not diff: {clean:?}");

    index.entries_mut().clear();
    index
        .entries_mut()
        .push(gitlink_entry("linked", submodule_tip));
    std::fs::create_dir_all(dir.path().join("linked")).map_err(grit_lib::error::Error::Io)?;
    let _ = diff_index_to_worktree_with_options(
        repo.odb(),
        &mut index,
        dir.path(),
        DiffIndexToWorktreeOptions {
            repository_git_dir: Some(repo.git_dir().to_path_buf()),
            simplify_gitlinks: false,
            ignore_submodule_untracked: false,
            ..DiffIndexToWorktreeOptions::default()
        },
    )?;
    Ok(())
}
