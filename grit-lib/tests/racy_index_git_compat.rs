//! Racy-git index mtime: grit-lib vs system `git` on the same working tree.
//!
//! Git can miss worktree changes when a file's stat data still matches the index
//! but the entry mtime is at or after the index file's mtime ("racy"). These tests
//! build repos with `git`, then compare `git diff` with [`grit_lib::diff::diff_index_to_worktree`].

use std::path::Path;

use filetime::FileTime;
use grit_lib::diff::diff_index_to_worktree;
use grit_lib::repo::Repository;
use grit_test_support::git;

fn pin_mtime(path: &Path, sec: u32, nsec: u32) {
    filetime::set_file_mtime(path, FileTime::from_unix_time(i64::from(sec), nsec))
        .expect("set mtime");
}

fn git_diff_name_only(repo: &Path) -> Vec<String> {
    let out = git(repo, &["diff", "--name-only"]);
    out.lines()
        .filter(|l| !l.is_empty())
        .map(str::to_owned)
        .collect()
}

#[test]
fn racy_same_size_change_matches_git_diff() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = tmp.path();
    git(repo, &["init", "-q", "-b", "main", "."]);
    git(repo, &["config", "user.email", "t@example.com"]);
    git(repo, &["config", "user.name", "Test"]);

    const CONTENT_A: &str = "same length!!";
    const CONTENT_B: &str = "same length??";
    assert_eq!(CONTENT_A.len(), CONTENT_B.len());

    std::fs::write(repo.join("f.txt"), CONTENT_A).expect("write");
    git(repo, &["add", "f.txt"]);
    git(repo, &["commit", "-qm", "c1"]);

    let grit_repo = Repository::open(&repo.join(".git"), Some(repo)).expect("open");
    let index = grit_repo.load_index().expect("load index");
    let index_mtime = index.source_mtime.expect("index source mtime from disk");
    let entry = index
        .entries()
        .iter()
        .find(|e| e.path == b"f.txt")
        .expect("index entry");
    let entry_mtime = (entry.mtime_sec, entry.mtime_nsec);

    std::fs::write(repo.join("f.txt"), CONTENT_B).expect("rewrite");
    pin_mtime(&repo.join("f.txt"), entry_mtime.0, entry_mtime.1);

    let git_names = git_diff_name_only(repo);
    assert_eq!(
        git_names,
        vec!["f.txt".to_owned()],
        "system git must detect same-size racy modification"
    );

    let grit_index = grit_repo.load_index().expect("reload index");
    assert_eq!(
        grit_index.source_mtime,
        Some(index_mtime),
        "source mtime preserved across reload"
    );
    let diff = diff_index_to_worktree(&grit_repo.odb, &grit_index, repo, false, false)
        .expect("diff_index_to_worktree");
    assert_eq!(diff.len(), 1, "grit must re-hash racy entry");
    assert_eq!(diff[0].path(), "f.txt");
}

#[test]
fn racy_unchanged_content_matches_git_clean_diff() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = tmp.path();
    git(repo, &["init", "-q", "-b", "main", "."]);
    git(repo, &["config", "user.email", "t@example.com"]);
    git(repo, &["config", "user.name", "Test"]);

    const CONTENT: &str = "unchanged!!!!";
    std::fs::write(repo.join("f.txt"), CONTENT).expect("write");
    git(repo, &["add", "f.txt"]);
    git(repo, &["commit", "-qm", "c1"]);

    let grit_repo = Repository::open(&repo.join(".git"), Some(repo)).expect("open");
    let index = grit_repo.load_index().expect("load index");
    let entry = index
        .entries()
        .iter()
        .find(|e| e.path == b"f.txt")
        .expect("index entry");
    let entry_mtime = (entry.mtime_sec, entry.mtime_nsec);

    pin_mtime(&repo.join("f.txt"), entry_mtime.0, entry_mtime.1);

    let git_names = git_diff_name_only(repo);
    assert!(
        git_names.is_empty(),
        "system git must treat racy clean file as unmodified: {git_names:?}"
    );

    let grit_index = grit_repo.load_index().expect("reload index");
    let diff = diff_index_to_worktree(&grit_repo.odb, &grit_index, repo, false, false)
        .expect("diff_index_to_worktree");
    assert!(
        diff.is_empty(),
        "grit must not report modification for racy clean content: {diff:?}"
    );
}
