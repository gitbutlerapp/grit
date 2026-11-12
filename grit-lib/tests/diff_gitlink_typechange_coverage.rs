//! Index/worktree diff when a gitlink path becomes a regular file (t4041-style).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use grit_lib::diff::{DiffStatus, SubmoduleIgnoreMode, TreeDiffOptions};
use grit_lib::index::{Index, IndexEntry};
use grit_lib::objects::FileMode;
use grit_lib::objects::{ObjectId, ObjectKind};
use grit_lib::repo::{InitOptions, Repository};
use tempfile::tempdir;

fn gitlink_entry(path: &str, oid: ObjectId) -> IndexEntry {
    IndexEntry::new(path.as_bytes().to_vec(), oid, FileMode::GITLINK)
}

#[test]
fn gitlink_replaced_by_regular_file_reports_typechange() -> grit_lib::error::Result<()> {
    let dir = tempdir().map_err(grit_lib::error::Error::Io)?;
    let (repo, _) = Repository::init(dir.path(), &InitOptions::default())?;
    let submodule_tip = repo
        .odb()
        .write(ObjectKind::Commit, b"tree 0000000000000000000000000000000000000000\nauthor T <t@e.com> 1 +0000\ncommitter T <t@e.com> 1 +0000\n\nsub\n")?;
    let mut index = Index::new();
    index
        .entries_mut()
        .push(gitlink_entry("submod", submodule_tip));
    std::fs::write(dir.path().join("submod"), b"now a file\n")
        .map_err(grit_lib::error::Error::Io)?;

    let diff = repo
        .diff()
        .index_to_worktree(
            &mut index,
            &TreeDiffOptions::default().with_submodules(SubmoduleIgnoreMode::Dirty),
            &mut grit_lib::progress::NullProgress,
            &grit_lib::progress::CancelToken::never(),
        )?
        .entries;
    assert!(
        diff.iter().any(|e| e.status == DiffStatus::TypeChanged && e.old_path.as_deref() == Some("submod")),
        "expected typechange for gitlink -> file, got {diff:?}"
    );

    let blob = repo.odb().write(ObjectKind::Blob, b"still file\n")?;
    index.entries_mut().clear();
    index
        .entries_mut()
        .push(IndexEntry::new(b"submod".to_vec(), blob, FileMode::REGULAR));
    std::fs::write(dir.path().join("submod"), b"still file\n")
        .map_err(grit_lib::error::Error::Io)?;
    let clean = repo
        .diff()
        .index_to_worktree(
            &mut index,
            &TreeDiffOptions::default(),
            &mut grit_lib::progress::NullProgress,
            &grit_lib::progress::CancelToken::never(),
        )?
        .entries;
    assert!(clean.is_empty(), "matching file should not diff: {clean:?}");

    index.entries_mut().clear();
    index
        .entries_mut()
        .push(gitlink_entry("linked", submodule_tip));
    std::fs::create_dir_all(dir.path().join("linked")).map_err(grit_lib::error::Error::Io)?;
    let _ = repo.diff().index_to_worktree(
        &mut index,
        &TreeDiffOptions::default(),
        &mut grit_lib::progress::NullProgress,
        &grit_lib::progress::CancelToken::never(),
    )?;
    Ok(())
}
