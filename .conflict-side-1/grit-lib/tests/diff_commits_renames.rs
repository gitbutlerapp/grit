//! Rename detection for [`grit_lib::diff::diff_commits_with_renames`] vs `git diff -M`.

use std::path::Path;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::diff::{diff_commits_with_renames, DiffStatus};
use grit_lib::objects::ObjectId;
use grit_lib::repo::Repository;
use grit_test_support::git;
use tempfile::TempDir;

fn init_repo(dir: &Path) {
    git(dir, &["init", "-q", "-b", "main", "."]);
    git(dir, &["config", "user.email", "u@example.com"]);
    git(dir, &["config", "user.name", "U"]);
}

fn commit_all(dir: &Path, msg: &str) -> ObjectId {
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-qm", msg]);
    git(dir, &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .expect("oid")
}

#[test]
fn diff_commits_with_renames_reports_pure_rename() {
    let dir = TempDir::new().expect("tempdir");
    init_repo(dir.path());
    std::fs::write(dir.path().join("a.txt"), "content\n").expect("write");
    let base = commit_all(dir.path(), "one");
    std::fs::create_dir(dir.path().join("d")).expect("mkdir");
    git(dir.path(), &["mv", "a.txt", "d/a.txt"]);
    let tip = commit_all(dir.path(), "moved");

    let git_status = git(
        dir.path(),
        &["diff", "-M", "--name-status", &base.to_hex(), &tip.to_hex()],
    );
    assert!(
        git_status.contains("R100\ta.txt\td/a.txt")
            || git_status.contains("R100\ta.txt\td/a.txt\n"),
        "git rename line missing: {git_status:?}"
    );

    let root = dir.path();
    let repo = Repository::open(
        &root.join(".git"),
        &RepositoryOptions::empty().with_work_tree(root),
    )
    .expect("open");
    let entries =
        diff_commits_with_renames(repo.odb(), &base, &tip, 50).expect("diff_commits_with_renames");
    assert_eq!(
        entries.len(),
        1,
        "expected one renamed entry, got {entries:?}"
    );
    let e = &entries[0];
    assert_eq!(e.status, DiffStatus::Renamed);
    assert_eq!(e.path(), "d/a.txt");
    assert_eq!(e.old_path.as_deref(), Some("a.txt"));
}
