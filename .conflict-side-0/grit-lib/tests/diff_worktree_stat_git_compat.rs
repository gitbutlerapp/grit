//! Index↔worktree diff must stat ordinary tracked paths (Git `diff-files` parity).

use std::process::Command;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::diff::{diff_index_to_worktree_with_options, DiffIndexToWorktreeOptions};
use grit_lib::repo::Repository;
use grit_test_support::git;

fn grit_unstaged_paths(repo: &Repository, root: &std::path::Path) -> Vec<String> {
    let mut index = repo.index_handle().load().expect("index");
    let index_mtime = grit_lib::diff::index_mtime_for_diff(&index, None);
    let config = repo.config().expect("config");
    let (entries, _) = diff_index_to_worktree_with_options(
        repo.odb(),
        &mut index,
        root,
        DiffIndexToWorktreeOptions {
            repository_git_dir: Some(repo.git_dir().to_path_buf()),
            index_mtime,
            refresh_index_stat_in_pass: false,
            for_status: true,
            config: Some(config),
            thread_policy: Some(repo.thread_policy()),
            ..DiffIndexToWorktreeOptions::default()
        },
    )
    .expect("diff");
    entries
        .into_iter()
        .filter_map(|e| e.old_path.or(e.new_path))
        .collect()
}

fn git_diff_files_name_only(root: &std::path::Path) -> Vec<String> {
    let out = git(root, &["diff-files", "--name-only"]);
    out.lines()
        .filter(|l| !l.is_empty())
        .map(str::to_owned)
        .collect()
}

fn open_repo(root: &std::path::Path) -> Repository {
    Repository::open(
        &root.join(".git"),
        &RepositoryOptions::empty().with_work_tree(root),
    )
    .expect("open")
}

#[test]
fn modified_tracked_file_after_index_matches_git_diff_files() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main", "."]);
    git(root, &["config", "user.email", "t@example.com"]);
    git(root, &["config", "user.name", "Test"]);
    std::fs::write(root.join("tracked.txt"), "index version\n").expect("write");
    git(root, &["add", "tracked.txt"]);
    git(root, &["commit", "-qm", "base"]);

    std::fs::write(root.join("tracked.txt"), "worktree edit\n").expect("rewrite");

    let git_names = git_diff_files_name_only(root);
    assert_eq!(git_names, vec!["tracked.txt".to_owned()]);

    let status = Command::new("git")
        .args(["-C", root.to_str().unwrap(), "diff", "--quiet"])
        .status()
        .expect("git diff --quiet");
    assert!(!status.success(), "git diff must report worktree change");

    let repo = open_repo(root);
    let grit_names = grit_unstaged_paths(&repo, root);
    assert_eq!(grit_names, git_names);
}

#[test]
fn deleted_tracked_file_after_index_matches_git_diff_files() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main", "."]);
    git(root, &["config", "user.email", "t@example.com"]);
    git(root, &["config", "user.name", "Test"]);
    std::fs::write(root.join("tracked.txt"), "keep me indexed\n").expect("write");
    git(root, &["add", "tracked.txt"]);
    git(root, &["commit", "-qm", "base"]);

    std::fs::remove_file(root.join("tracked.txt")).expect("delete");

    let git_names = git_diff_files_name_only(root);
    assert_eq!(git_names, vec!["tracked.txt".to_owned()]);

    let repo = open_repo(root);
    let grit_names = grit_unstaged_paths(&repo, root);
    assert_eq!(grit_names, git_names);
}
