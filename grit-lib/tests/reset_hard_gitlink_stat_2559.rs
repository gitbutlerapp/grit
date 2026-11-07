//! Regression for issue #2559: hard reset at HEAD on a clean tree must not rewrite
//! tracked blobs after a gitlink (or skip-worktree hole) in the index.

use std::fs;
use std::path::Path;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::porcelain::reset::{reset_to_commit, ResetMode, ResetOptions};
use grit_lib::progress::{CancelToken, NullProgress};
use grit_lib::repo::Repository;
use grit_test_support::{git, git_cmd};
use tempfile::TempDir;

fn ident() -> grit_lib::ident::Signature {
    "T <t@e.c> 1700000000 +0000".parse().expect("signature")
}

fn hard_reset_to_head(dir: &Path) {
    let repo =
        Repository::discover(Some(dir), &RepositoryOptions::empty()).expect("open repository");
    let head = grit_test_support::rev_parse(dir, "HEAD");
    reset_to_commit(
        &repo,
        &ResetOptions::new(head, ResetMode::Hard, ident(), "reset: moving to HEAD"),
        &mut NullProgress,
        &CancelToken::never(),
    )
    .expect("hard reset");
}

fn init_repo(dir: &Path) {
    git(dir, &["init", "-q", "-b", "main"]);
    git(dir, &["config", "user.name", "T"]);
    git(dir, &["config", "user.email", "t@e.c"]);
}

#[cfg(unix)]
fn tracked_file_identity(path: &Path) -> (u64, u64, filetime::FileTime) {
    use std::os::unix::fs::MetadataExt;
    let meta = fs::metadata(path).expect("stat");
    (
        meta.dev(),
        meta.ino(),
        filetime::FileTime::from_last_modification_time(&meta),
    )
}

fn init_gitlink_fixture(dir: &Path, skip_worktree_on_a10: bool) {
    init_repo(dir);
    for i in 1..=10 {
        fs::write(dir.join(format!("a{i}")), format!("{i}\n")).expect("write");
    }
    for i in 1..=120 {
        fs::write(dir.join(format!("z{i}")), format!("{i}\n")).expect("write");
    }
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-qm", "files"]);
    let head = git(dir, &["rev-parse", "HEAD"]).trim().to_string();
    git(
        dir,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{head},m_sub"),
        ],
    );
    git(dir, &["commit", "-qm", "gitlink"]);
    fs::create_dir_all(dir.join("m_sub")).expect("gitlink dir");
    if skip_worktree_on_a10 {
        git(dir, &["update-index", "--skip-worktree", "a10"]);
    }
    assert!(
        git_cmd(&["status", "--porcelain"])
            .in_dir(dir)
            .suc()
            .stdout
            .trim()
            .is_empty(),
        "fixture must start clean"
    );
}

#[test]
#[cfg(unix)]
fn hard_reset_at_head_preserves_identities_with_gitlink() {
    let dir = TempDir::new().expect("tempdir");
    init_gitlink_fixture(dir.path(), false);
    let wt = dir.path();
    let before: Vec<(String, (u64, u64, filetime::FileTime))> = (1..=120)
        .map(|i| {
            let rel = format!("z{i}");
            let path = wt.join(&rel);
            (rel, tracked_file_identity(&path))
        })
        .collect();

    hard_reset_to_head(wt);

    for (rel, (dev, ino, mtime)) in before {
        let path = wt.join(&rel);
        let after = tracked_file_identity(&path);
        assert_eq!(
            (after.0, after.1),
            (dev, ino),
            "hard reset at HEAD must not rewrite {rel} (inode)"
        );
        assert_eq!(
            after.2, mtime,
            "hard reset at HEAD must not touch mtime of {rel}"
        );
    }
    assert!(git_cmd(&["status", "--porcelain", "--untracked-files=no"])
        .in_dir(wt)
        .suc()
        .stdout
        .trim()
        .is_empty());
    git(wt, &["fsck", "--no-progress"]);
}

#[test]
#[cfg(unix)]
fn hard_reset_at_head_preserves_identities_with_skip_worktree_before_gitlink() {
    let dir = TempDir::new().expect("tempdir");
    init_gitlink_fixture(dir.path(), true);
    let wt = dir.path();
    let before: Vec<(String, (u64, u64, filetime::FileTime))> = (1..=120)
        .map(|i| {
            let rel = format!("z{i}");
            let path = wt.join(&rel);
            (rel, tracked_file_identity(&path))
        })
        .collect();

    hard_reset_to_head(wt);

    for (rel, (dev, ino, mtime)) in before {
        let path = wt.join(&rel);
        let after = tracked_file_identity(&path);
        assert_eq!(
            (after.0, after.1),
            (dev, ino),
            "hard reset at HEAD must not rewrite {rel} (inode)"
        );
        assert_eq!(
            after.2, mtime,
            "hard reset at HEAD must not touch mtime of {rel}"
        );
    }
    assert!(git_cmd(&["status", "--porcelain", "--untracked-files=no"])
        .in_dir(wt)
        .suc()
        .stdout
        .trim()
        .is_empty());
}
