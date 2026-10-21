//! Rename detection for index↔HEAD and index↔worktree diffs (issue #1012).

use std::io::Write;
use std::path::Path;
use std::process::Command;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::diff::tree_oid_of_commit;
use grit_lib::diff::{
    apply_rename_detection_to_diff_entries, diff_index_to_tree_with_renames,
    diff_tree_to_worktree_with_git_dir, DiffStatus,
};
use grit_lib::repo::Repository;
use grit_lib::state::resolve_head;

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

fn init_repo_with_large_file(root: &Path) {
    git_run(root, &["init", "-q", "-b", "main"]);
    let body: String = (1..=40)
        .map(|i| i.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(root.join("a.txt"), format!("{body}\n")).unwrap();
    git_run(root, &["add", "."]);
    git_run(root, &["commit", "-qm", "one"]);
}

#[test]
fn diff_index_to_tree_with_renames_reports_staged_mv() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    init_repo_with_large_file(root);
    git_run(root, &["mv", "a.txt", "b.txt"]);

    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let head = resolve_head(repo.git_dir()).expect("head");
    let head_commit = head.oid().expect("commit");
    let head_tree = tree_oid_of_commit(repo.odb(), head_commit).expect("tree");
    let index = repo.index_handle().load().expect("index");
    let config = repo.config().expect("config");
    let policy = repo.thread_policy();

    let entries = diff_index_to_tree_with_renames(
        repo.odb(),
        &index,
        Some(&head_tree),
        false,
        50,
        config.as_ref(),
        &policy,
    )
    .expect("staged diff");

    let renames: Vec<_> = entries
        .iter()
        .filter(|e| e.status == DiffStatus::Renamed)
        .collect();
    assert_eq!(
        renames.len(),
        1,
        "expected one staged rename, got: {entries:?}"
    );
    assert_eq!(renames[0].old_path.as_deref(), Some("a.txt"));
    assert_eq!(renames[0].new_path.as_deref(), Some("b.txt"));
}

#[test]
fn apply_rename_detection_matches_git_cached_name_status() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    init_repo_with_large_file(root);
    git_run(root, &["mv", "a.txt", "b.txt"]);

    let git_out = Command::new("git")
        .current_dir(root)
        .args(["diff", "--cached", "--name-status", "-M"])
        .output()
        .expect("git diff");
    assert!(git_out.status.success());
    let git_line = String::from_utf8_lossy(&git_out.stdout);
    assert!(
        git_line.contains("R100\t") || git_line.contains("R"),
        "git should report rename: {git_line}"
    );

    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let head = resolve_head(repo.git_dir()).expect("head");
    let head_commit = head.oid().expect("commit");
    let head_tree = tree_oid_of_commit(repo.odb(), head_commit).expect("tree");
    let index = repo.index_handle().load().expect("index");
    let config = repo.config().expect("config");
    let policy = repo.thread_policy();
    let raw = grit_lib::diff::diff_index_to_tree(repo.odb(), &index, Some(&head_tree), false)
        .expect("raw");
    let entries =
        apply_rename_detection_to_diff_entries(repo.odb(), raw, None, 50, config.as_ref(), &policy)
            .expect("renames");
    assert!(
        entries.iter().any(|e| e.status == DiffStatus::Renamed),
        "grit should pair rename like git: {entries:?}"
    );
}

#[test]
fn worktree_rename_detection_with_small_unstaged_edit() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    init_repo_with_large_file(root);
    git_run(root, &["mv", "a.txt", "b.txt"]);
    std::fs::OpenOptions::new()
        .append(true)
        .open(root.join("b.txt"))
        .expect("open b.txt")
        .write_all(b"41\n")
        .expect("append");

    let git_out = Command::new("git")
        .current_dir(root)
        .args(["diff", "HEAD", "--name-status", "-M"])
        .output()
        .expect("git diff");
    assert!(git_out.status.success());
    let git_line = String::from_utf8_lossy(&git_out.stdout);
    assert!(
        git_line.starts_with('R'),
        "git should report inexact rename, got: {git_line}"
    );

    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let head = resolve_head(repo.git_dir()).expect("head");
    let head_commit = head.oid().expect("commit");
    let head_tree = tree_oid_of_commit(repo.odb(), head_commit).expect("tree");
    let index = repo.index_handle().load().expect("index");
    let config = repo.config().expect("config");
    let policy = repo.thread_policy();
    let work_tree = repo.work_tree().expect("worktree");

    let raw = diff_tree_to_worktree_with_git_dir(
        repo.odb(),
        Some(&head_tree),
        work_tree,
        repo.git_dir(),
        &index,
        Some(config.as_ref()),
    )
    .expect("tree to worktree");
    let entries = apply_rename_detection_to_diff_entries(
        repo.odb(),
        raw,
        Some(work_tree),
        50,
        config.as_ref(),
        &policy,
    )
    .expect("renames");

    let renames: Vec<_> = entries
        .iter()
        .filter(|e| e.status == DiffStatus::Renamed)
        .collect();
    assert_eq!(
        renames.len(),
        1,
        "expected one worktree rename, got: {entries:?}"
    );
    assert_eq!(renames[0].old_path.as_deref(), Some("a.txt"));
    assert_eq!(renames[0].new_path.as_deref(), Some("b.txt"));
    assert!(
        renames[0].score.is_some_and(|s| s >= 50),
        "rename score should meet -M50 threshold: {:?}",
        renames[0].score
    );
}

#[test]
fn plain_filesystem_mv_pairs_via_untracked_worktree_candidate() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    init_repo_with_large_file(root);
    std::fs::rename(root.join("a.txt"), root.join("b.txt")).expect("mv");

    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let mut index = repo.index_handle().load().expect("index");
    let work_tree = repo.work_tree().expect("worktree");
    let rules = grit_lib::worktree::worktree_rules::WorktreeRules::from_repository(&repo, &index)
        .expect("rules");

    let (entries, _) = grit_lib::diff::diff_index_to_worktree_with_renames(
        &repo,
        &mut index,
        work_tree,
        grit_lib::diff::DiffIndexToWorktreeOptions {
            repository_git_dir: Some(repo.git_dir().to_path_buf()),
            config: repo.config().ok(),
            worktree_rules: Some(std::sync::Arc::new(std::sync::Mutex::new(rules))),
            ..grit_lib::diff::DiffIndexToWorktreeOptions::default()
        },
        50,
    )
    .expect("unstaged diff");

    let renames: Vec<_> = entries
        .iter()
        .filter(|e| e.status == DiffStatus::Renamed)
        .collect();
    assert_eq!(
        renames.len(),
        1,
        "plain mv should rename via untracked b.txt, got: {entries:?}"
    );
    assert_eq!(renames[0].old_path.as_deref(), Some("a.txt"));
    assert_eq!(renames[0].new_path.as_deref(), Some("b.txt"));
}

#[test]
fn unrelated_untracked_not_listed_when_tracked_file_deleted() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    init_repo_with_large_file(root);
    std::fs::remove_file(root.join("a.txt")).expect("delete tracked");
    std::fs::write(root.join("unrelated.txt"), "noise\n").expect("untracked");

    let git_out = Command::new("git")
        .current_dir(root)
        .args(["diff", "--name-status"])
        .output()
        .expect("git diff");
    assert!(git_out.status.success());
    let git_line = String::from_utf8_lossy(&git_out.stdout);
    assert!(
        !git_line.contains("unrelated.txt"),
        "git should not list unrelated untracked in diff: {git_line}"
    );
    assert!(
        git_line.contains('D'),
        "git should show deletion: {git_line}"
    );

    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let mut index = repo.index_handle().load().expect("index");
    let work_tree = repo.work_tree().expect("worktree");
    let rules = grit_lib::worktree::worktree_rules::WorktreeRules::from_repository(&repo, &index)
        .expect("rules");

    let (entries, _) = grit_lib::diff::diff_index_to_worktree_with_renames(
        &repo,
        &mut index,
        work_tree,
        grit_lib::diff::DiffIndexToWorktreeOptions {
            repository_git_dir: Some(repo.git_dir().to_path_buf()),
            config: repo.config().ok(),
            worktree_rules: Some(std::sync::Arc::new(std::sync::Mutex::new(rules))),
            ..grit_lib::diff::DiffIndexToWorktreeOptions::default()
        },
        50,
    )
    .expect("unstaged diff");

    assert!(
        !entries.iter().any(|e| e.path() == "unrelated.txt"),
        "unrelated untracked must not appear in diff: {entries:?}"
    );
    assert_eq!(
        entries
            .iter()
            .filter(|e| e.status == DiffStatus::Deleted)
            .count(),
        1,
        "expected single deletion: {entries:?}"
    );
}
