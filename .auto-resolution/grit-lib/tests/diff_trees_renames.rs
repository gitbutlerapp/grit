//! Public API coverage for [`grit_lib::diff::diff_trees_with_renames`].

use std::path::Path;
use std::process::Command;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::diff::{diff_trees_with_renames, DiffStatus};
use grit_lib::objects::ObjectId;
use grit_lib::repo::Repository;

fn git_run(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Diff Test")
        .env("GIT_AUTHOR_EMAIL", "diff@example.com")
        .env("GIT_COMMITTER_NAME", "Diff Test")
        .env("GIT_COMMITTER_EMAIL", "diff@example.com")
        .args(args)
        .output()
        .expect("spawn git");
    assert!(
        out.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
}

fn rev_parse_tree(dir: &Path, rev: &str) -> ObjectId {
    let out = Command::new("git")
        .current_dir(dir)
        .args(["rev-parse", rev])
        .output()
        .expect("rev-parse");
    assert!(out.status.success());
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .expect("oid")
}

#[test]
fn diff_trees_with_renames_pairs_rename_and_content_delta() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git_run(root, &["init", "-q", "-b", "main"]);

    let body: String = (1..=20)
        .map(|i| format!("line {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(root.join("old.txt"), format!("{body}\n")).unwrap();
    git_run(root, &["add", "old.txt"]);
    git_run(root, &["commit", "-m", "init"]);

    let mut edited = body.clone();
    edited = edited.replacen("line 5", "line five", 1);
    std::fs::write(root.join("old.txt"), format!("{edited}\n")).unwrap();
    git_run(root, &["mv", "old.txt", "new.txt"]);
    git_run(root, &["add", "new.txt"]);
    git_run(root, &["commit", "-m", "rename with one-line edit"]);

    let parent_tree = rev_parse_tree(root, "HEAD~1^{tree}");
    let commit_tree = rev_parse_tree(root, "HEAD^{tree}");

    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let entries =
        diff_trees_with_renames(repo.odb(), Some(&parent_tree), Some(&commit_tree), "", 50)
            .expect("diff_trees_with_renames");

    let renames: Vec<_> = entries
        .iter()
        .filter(|e| e.status == DiffStatus::Renamed)
        .collect();
    assert_eq!(
        renames.len(),
        1,
        "expected one rename entry, got: {entries:?}"
    );
    let rn = renames[0];
    assert_eq!(rn.old_path.as_deref(), Some("old.txt"));
    assert_eq!(rn.new_path.as_deref(), Some("new.txt"));
    assert!(
        rn.score.is_some_and(|s| s >= 50),
        "rename score should reflect high similarity: {:?}",
        rn.score
    );

    let deleted = entries
        .iter()
        .filter(|e| e.status == DiffStatus::Deleted)
        .count();
    let added = entries
        .iter()
        .filter(|e| e.status == DiffStatus::Added)
        .count();
    assert_eq!(
        deleted, 0,
        "rename detection should not leave a separate delete: {entries:?}"
    );
    assert_eq!(
        added, 0,
        "rename detection should not leave a separate add: {entries:?}"
    );
}
