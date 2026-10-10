//! `remove_paths` / `move_path` compatibility with the system `git` binary.

use std::process::Command;

use grit_lib::porcelain::paths::{move_path, remove_paths, RemoveOptions};
use grit_lib::repo::Repository;
use grit_lib::write_tree::{cache_tree_fully_valid, write_tree_update_index, WriteTreeFlags};
use grit_test_support::git;

fn git_porcelain(repo: &std::path::Path) -> String {
    let out = Command::new("git")
        .current_dir(repo)
        .args(["status", "--porcelain"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git status");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).to_string()
}

fn git_diff_cached_rename(repo: &std::path::Path) -> bool {
    let out = Command::new("git")
        .current_dir(repo)
        .args(["diff", "--cached", "-M"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git diff");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout);
    text.contains("rename from") || text.contains("similarity index")
}

fn fsck_strict(repo: &std::path::Path) {
    let out = Command::new("git")
        .current_dir(repo)
        .args(["fsck", "--strict"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git fsck");
    assert!(
        out.status.success(),
        "git fsck --strict: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn commit_all(repo: &std::path::Path, msg: &str) {
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "-q", "-m", msg]);
}

fn grit_repo(dir: &std::path::Path) -> Repository {
    Repository::discover(Some(dir)).expect("open repo")
}

#[test]
fn rm_clean_file_staged_deletion() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo_dir = tmp.path();
    git(repo_dir, &["init", "-q", "-b", "main"]);
    std::fs::write(repo_dir.join("keep.txt"), "keep\n").unwrap();
    std::fs::write(repo_dir.join("gone.txt"), "gone\n").unwrap();
    commit_all(repo_dir, "init");

    let repo = grit_repo(repo_dir);
    remove_paths(
        &repo,
        &RemoveOptions {
            pathspecs: vec!["gone.txt".into()],
            pathspec_sources: vec!["gone.txt".into()],
            ..Default::default()
        },
    )
    .expect("rm");

    assert!(!repo_dir.join("gone.txt").exists());
    let status = git_porcelain(repo_dir);
    assert!(
        status.contains("D  gone.txt") || status.contains("D gone.txt"),
        "expected staged deletion, got:\n{status}"
    );
}

#[test]
fn rm_cached_leaves_untracked_file() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo_dir = tmp.path();
    git(repo_dir, &["init", "-q", "-b", "main"]);
    std::fs::write(repo_dir.join("stay.txt"), "stay\n").unwrap();
    commit_all(repo_dir, "init");

    let repo = grit_repo(repo_dir);
    remove_paths(
        &repo,
        &RemoveOptions {
            pathspecs: vec!["stay.txt".into()],
            pathspec_sources: vec!["stay.txt".into()],
            cached: true,
            ..Default::default()
        },
    )
    .expect("rm --cached");

    assert!(repo_dir.join("stay.txt").is_file());
    let status = git_porcelain(repo_dir);
    assert!(
        status.contains("?? stay.txt"),
        "expected untracked file, got:\n{status}"
    );
}

#[test]
fn rm_modified_refused_without_force() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo_dir = tmp.path();
    git(repo_dir, &["init", "-q", "-b", "main"]);
    std::fs::write(repo_dir.join("dirty.txt"), "v1\n").unwrap();
    commit_all(repo_dir, "init");
    std::fs::write(repo_dir.join("dirty.txt"), "v2\n").unwrap();

    let repo = grit_repo(repo_dir);
    let err = remove_paths(
        &repo,
        &RemoveOptions {
            pathspecs: vec!["dirty.txt".into()],
            pathspec_sources: vec!["dirty.txt".into()],
            ..Default::default()
        },
    )
    .expect_err("rm should refuse");
    assert!(matches!(
        err,
        grit_lib::error::Error::PathsHaveLocalModifications { .. }
    ));
}

#[test]
fn rm_recursive_directory() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo_dir = tmp.path();
    git(repo_dir, &["init", "-q", "-b", "main"]);
    std::fs::create_dir_all(repo_dir.join("tree/sub")).unwrap();
    std::fs::write(repo_dir.join("tree/sub/a.txt"), "a\n").unwrap();
    std::fs::write(repo_dir.join("tree/sub/b.txt"), "b\n").unwrap();
    commit_all(repo_dir, "init");

    let repo = grit_repo(repo_dir);
    remove_paths(
        &repo,
        &RemoveOptions {
            pathspecs: vec!["tree".into()],
            pathspec_sources: vec!["tree/".into()],
            recursive: true,
            ..Default::default()
        },
    )
    .expect("rm -r");

    assert!(!repo_dir.join("tree").exists());
    let status = git_porcelain(repo_dir);
    assert!(status.contains("D  tree/sub/a.txt") || status.contains("D tree/sub/a.txt"));
    assert!(status.contains("tree/sub/b.txt"));
}

#[test]
fn mv_file_detected_as_rename_by_git() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo_dir = tmp.path();
    git(repo_dir, &["init", "-q", "-b", "main"]);
    std::fs::write(repo_dir.join("old.txt"), "payload\n").unwrap();
    commit_all(repo_dir, "init");

    let repo = grit_repo(repo_dir);
    move_path(&repo, "old.txt", "new.txt", false).expect("mv");

    let status = git_porcelain(repo_dir);
    assert!(
        status.contains("R ") || (status.contains("D ") && status.contains("A ")),
        "expected rename or delete+add:\n{status}"
    );
    assert!(git_diff_cached_rename(repo_dir));
    assert!(repo_dir.join("new.txt").is_file());
    assert!(!repo_dir.join("old.txt").exists());
}

#[test]
fn mv_directory_tree() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo_dir = tmp.path();
    git(repo_dir, &["init", "-q", "-b", "main"]);
    std::fs::create_dir_all(repo_dir.join("src/lib")).unwrap();
    std::fs::write(repo_dir.join("src/lib/mod.rs"), "mod\n").unwrap();
    commit_all(repo_dir, "init");

    let repo = grit_repo(repo_dir);
    move_path(&repo, "src", "vendor/src", false).expect("mv dir");

    assert!(repo_dir.join("vendor/src/lib/mod.rs").is_file());
    assert!(!repo_dir.join("src").exists());
}

#[test]
fn mv_destination_exists_refused() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo_dir = tmp.path();
    git(repo_dir, &["init", "-q", "-b", "main"]);
    std::fs::write(repo_dir.join("a.txt"), "a\n").unwrap();
    std::fs::write(repo_dir.join("b.txt"), "b\n").unwrap();
    commit_all(repo_dir, "init");

    let repo = grit_repo(repo_dir);
    let err = move_path(&repo, "a.txt", "b.txt", false).expect_err("mv");
    assert!(matches!(err, grit_lib::error::Error::Message(_)));
}

#[test]
fn cache_tree_valid_after_mv_commit() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo_dir = tmp.path();
    git(repo_dir, &["init", "-q", "-b", "main"]);
    std::fs::write(repo_dir.join("one.txt"), "1\n").unwrap();
    std::fs::write(repo_dir.join("two.txt"), "2\n").unwrap();
    commit_all(repo_dir, "init");

    let repo = grit_repo(repo_dir);
    move_path(&repo, "one.txt", "three.txt", false).expect("mv");

    let mut index = repo.load_index().expect("index");
    let _tree = write_tree_update_index(&repo.odb, &mut index, "", WriteTreeFlags::default())
        .expect("write-tree");
    assert!(cache_tree_fully_valid(&repo.odb, index.cache_tree.as_ref()));
    repo.write_index(&mut index).expect("write index");

    git(repo_dir, &["commit", "-q", "-m", "rename"]);
    fsck_strict(repo_dir);
}

#[test]
fn grit_rm_after_git_staged_rename() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo_dir = tmp.path();
    git(repo_dir, &["init", "-q", "-b", "main"]);
    std::fs::write(repo_dir.join("tracked.txt"), "body\n").unwrap();
    commit_all(repo_dir, "init");
    git(repo_dir, &["mv", "tracked.txt", "renamed.txt"]);

    let repo = grit_repo(repo_dir);
    remove_paths(
        &repo,
        &RemoveOptions {
            pathspecs: vec!["renamed.txt".into()],
            pathspec_sources: vec!["renamed.txt".into()],
            force: true,
            ..Default::default()
        },
    )
    .expect("grit rm -f after git mv");

    assert!(!repo_dir.join("renamed.txt").exists());
    let status = git_porcelain(repo_dir);
    assert!(
        status.contains("D  tracked.txt") || status.contains("D tracked.txt"),
        "git rm -f after a staged rename stages deletion of the old path:\n{status}"
    );
}
