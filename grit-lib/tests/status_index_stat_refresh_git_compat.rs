//! Status persists stat-refreshed index entries after mtime-only worktree bumps (issue #924).
//!
//! After checkout or branch switch, Git often leaves tracked files with newer mtimes while
//! content still matches the index. `git status` re-verifies content once, writes the refreshed
//! stat data back to `.git/index`, and later status runs trust stat without re-hashing.

use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use grit_lib::diff::stat_matches;
use grit_lib::index::Index;
use grit_lib::porcelain::status::{status, StatusOptions};
use grit_lib::progress::NullProgress;
use grit_lib::repo::Repository;
use grit_test_support::git;

fn sparse_directory_placeholder_count(index: &Index) -> usize {
    index
        .entries
        .iter()
        .filter(|e| e.is_sparse_directory_placeholder())
        .count()
}

fn touch_all_tracked(repo: &Path) {
    let out = git(repo, &["ls-files", "-z"]);
    for path in out.split('\0').filter(|p| !p.is_empty()) {
        let abs = repo.join(path);
        let now = filetime::FileTime::now();
        filetime::set_file_mtime(&abs, now).expect("touch tracked file");
    }
}

#[test]
fn status_refreshes_index_stat_after_touch_and_second_run_is_faster() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo_root = tmp.path();
    git(repo_root, &["init", "-q", "-b", "main", "."]);
    git(repo_root, &["config", "user.email", "t@example.com"]);
    git(repo_root, &["config", "user.name", "Test"]);

    const FILE_COUNT: usize = 80;
    for i in 0..FILE_COUNT {
        let name = format!("file-{i:03}.txt");
        fs::write(repo_root.join(&name), format!("content {i}\n")).expect("write");
    }
    git(repo_root, &["add", "."]);
    git(repo_root, &["commit", "-qm", "initial"]);

    touch_all_tracked(repo_root);

    let grit_repo =
        Repository::open(&repo_root.join(".git"), Some(repo_root)).expect("open grit repo");
    let index_path = grit_repo.index_path();
    let index_mtime_before = fs::metadata(&index_path)
        .expect("index metadata")
        .modified()
        .expect("index mtime");

    let opts = StatusOptions::default();
    let first_start = Instant::now();
    let model = status(&grit_repo, &opts, &mut NullProgress).expect("first status");
    let first_elapsed = first_start.elapsed();
    assert!(
        model.unstaged.is_empty(),
        "touched files with unchanged content must not appear unstaged"
    );

    let index_mtime_after = fs::metadata(&index_path)
        .expect("index metadata")
        .modified()
        .expect("index mtime");
    assert!(
        index_mtime_after > index_mtime_before,
        "status must write refreshed stat data to the index"
    );

    let reloaded = grit_repo.load_index().expect("reload index");
    for entry in &reloaded.entries {
        if entry.stage() != 0 {
            continue;
        }
        let rel = std::str::from_utf8(&entry.path).expect("utf-8 path");
        let meta = fs::symlink_metadata(repo_root.join(rel)).expect("worktree metadata");
        assert!(
            stat_matches(entry, &meta),
            "index entry stat must match worktree after refresh for {rel}"
        );
    }

    let git_diff = git(repo_root, &["diff-files"]);
    assert!(
        git_diff.trim().is_empty(),
        "system git diff-files must be clean after grit refreshed the index"
    );

    let second_start = Instant::now();
    let _second = status(&grit_repo, &opts, &mut NullProgress).expect("second status");
    let second_elapsed = second_start.elapsed();

    assert!(
        second_elapsed * 3 + Duration::from_millis(5) < first_elapsed,
        "second status should avoid full re-hash (first {:?}, second {:?})",
        first_elapsed,
        second_elapsed
    );
}

#[test]
fn status_succeeds_when_index_lock_held_after_touch() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo_root = tmp.path();
    git(repo_root, &["init", "-q", "-b", "main", "."]);
    git(repo_root, &["config", "user.email", "t@example.com"]);
    git(repo_root, &["config", "user.name", "Test"]);

    fs::write(repo_root.join("tracked.txt"), "same content\n").expect("write");
    git(repo_root, &["add", "tracked.txt"]);
    git(repo_root, &["commit", "-qm", "initial"]);

    filetime::set_file_mtime(repo_root.join("tracked.txt"), filetime::FileTime::now())
        .expect("touch");

    let git_dir = repo_root.join(".git");
    let lock_path = git_dir.join("index.lock");
    fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&lock_path)
        .expect("pre-create index.lock");

    let git_exit = Command::new("git")
        .arg("status")
        .current_dir(repo_root)
        .status()
        .expect("git status");
    assert!(
        git_exit.success(),
        "system git status must succeed with index.lock"
    );

    let grit_repo = Repository::open(&git_dir, Some(repo_root)).expect("open grit repo");
    let index_path = grit_repo.index_path();
    let index_mtime_before = fs::metadata(&index_path)
        .expect("index metadata")
        .modified()
        .expect("index mtime");

    let model = status(&grit_repo, &StatusOptions::default(), &mut NullProgress)
        .expect("grit status must succeed when index.lock blocks refresh write");

    assert!(
        model.unstaged.is_empty(),
        "unchanged content after touch must not appear unstaged"
    );

    let index_mtime_after = fs::metadata(&index_path)
        .expect("index metadata")
        .modified()
        .expect("index mtime");
    assert_eq!(
        index_mtime_after, index_mtime_before,
        "index must not be rewritten while index.lock is held"
    );

    let _ = fs::remove_file(&lock_path);
}

#[test]
fn status_on_git_created_sparse_index_after_touch() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo_root = tmp.path().join("gg");
    let clone = Command::new("git")
        .args(["clone", "--depth", "1", "https://github.com/git/git"])
        .arg(&repo_root)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .status()
        .expect("spawn git clone");
    assert!(
        clone.success(),
        "git clone git.git for sparse-index fixture"
    );

    git(&repo_root, &["config", "user.email", "t@example.com"]);
    git(&repo_root, &["config", "user.name", "Test"]);
    git(&repo_root, &["update-index", "--refresh"]);
    git(
        &repo_root,
        &["sparse-checkout", "init", "--cone", "--sparse-index"],
    );
    git(&repo_root, &["sparse-checkout", "set", "Documentation"]);

    for path in git(&repo_root, &["ls-files", "Documentation"])
        .lines()
        .filter(|l| !l.is_empty())
        .take(64)
    {
        filetime::set_file_mtime(repo_root.join(path), filetime::FileTime::now())
            .expect("touch materialized sparse file");
    }

    let git_dir = repo_root.join(".git");
    let grit_repo = Repository::open(&git_dir, Some(&repo_root)).expect("open grit repo");
    let index_before = Index::load(&grit_repo.index_path()).expect("load sparse index");
    let placeholders_before = sparse_directory_placeholder_count(&index_before);
    assert!(
        placeholders_before > 0,
        "Git cone sparse-index must leave sparse-directory placeholders in the index"
    );

    let had_cache_tree_test = std::env::var("GIT_TEST_CHECK_CACHE_TREE").is_ok();
    std::env::set_var("GIT_TEST_CHECK_CACHE_TREE", "1");

    let model = status(&grit_repo, &StatusOptions::default(), &mut NullProgress)
        .expect("status must succeed on Git sparse index after mtime-only touches");

    if !had_cache_tree_test {
        std::env::remove_var("GIT_TEST_CHECK_CACHE_TREE");
    }

    assert!(
        model.unstaged.is_empty(),
        "mtime-only touches on materialized sparse paths must not appear unstaged"
    );

    let index_after = Index::load(&grit_repo.index_path()).expect("reload index after status");
    assert_eq!(
        sparse_directory_placeholder_count(&index_after),
        placeholders_before,
        "status stat refresh must not expand or drop sparse-directory placeholders"
    );

    let git_diff_files = git(&repo_root, &["diff-files"]);
    assert!(
        git_diff_files.trim().is_empty(),
        "system git diff-files must be clean after grit refreshed stats"
    );
}
