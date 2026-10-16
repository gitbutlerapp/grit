//! Fused pathless `git add` vs system Git (`ls-files -s`, `fsck`).

mod support;

use std::fs;
use std::process::Command;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::porcelain::add::{stage, StageMode, StageOptions};
use grit_lib::progress::NullProgress;
use grit_lib::repo::Repository;

use support::{assert_git_fsck_strict, assert_grit_index_matches_git_ls_files, git};

const FIXED_GIT_TIME: &str = "2005-04-07T22:13:13 +0000";

fn git_at_fixed_time(dir: &std::path::Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_DATE", FIXED_GIT_TIME)
        .env("GIT_COMMITTER_DATE", FIXED_GIT_TIME)
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {:?} in {}: {}",
        args,
        dir.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn ls_files_stage_lines(repo: &std::path::Path) -> Vec<String> {
    let out = Command::new("git")
        .args(["ls-files", "-s"])
        .current_dir(repo)
        .output()
        .expect("git ls-files -s");
    assert!(
        out.status.success(),
        "git ls-files -s: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let mut lines: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::to_owned)
        .collect();
    lines.sort();
    lines
}

fn grit_add_all(repo_root: &std::path::Path) {
    let repo = Repository::discover(Some(repo_root), &RepositoryOptions::empty()).expect("open");
    let mut options = StageOptions::default();
    options.mode = StageMode::All;
    stage(&repo, &options, &mut NullProgress).expect("grit add all");
}

fn parallel_repos(seed: impl FnOnce(&std::path::Path, &std::path::Path)) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let git_root = tmp.path().join("git");
    let grit_root = tmp.path().join("grit");
    fs::create_dir_all(&git_root).unwrap();
    fs::create_dir_all(&grit_root).unwrap();
    git(&git_root, &["init", "-q", "-b", "main", "."]);
    git(&grit_root, &["init", "-q", "-b", "main", "."]);
    for root in [&git_root, &grit_root] {
        git(root, &["config", "user.email", "t@example.com"]);
        git(root, &["config", "user.name", "Test"]);
    }
    seed(&git_root, &grit_root);
    git(&git_root, &["add", "-A"]);
    grit_add_all(&grit_root);
    let git_lines = ls_files_stage_lines(&git_root);
    let grit_lines = ls_files_stage_lines(&grit_root);
    assert_eq!(
        git_lines, grit_lines,
        "grit index must match git ls-files -s after add -A"
    );
    let grit_repo =
        Repository::discover(Some(&grit_root), &RepositoryOptions::empty()).expect("open");
    let index = grit_repo.index_handle().load().expect("index");
    assert_grit_index_matches_git_ls_files(&grit_root, &index);
    assert_git_fsck_strict(&git_root);
    assert_git_fsck_strict(&grit_root);
}

fn init_nested_repo(root: &std::path::Path, name: &str) {
    let nested = root.join(name);
    fs::create_dir_all(&nested).unwrap();
    git(&nested, &["init", "-q", "-b", "main", "."]);
    git(&nested, &["config", "user.email", "t@example.com"]);
    git(&nested, &["config", "user.name", "Test"]);
    fs::write(nested.join("inner.txt"), b"inner\n").unwrap();
    git(&nested, &["add", "inner.txt"]);
    git_at_fixed_time(&nested, &["commit", "-qm", "inner"]);
}

#[test]
fn fused_add_matches_git_untracked_beside_gitlink_in_parent() {
    parallel_repos(|git_root, grit_root| {
        init_nested_repo(git_root, "packages/sub");
        init_nested_repo(grit_root, "packages/sub");
        git(git_root, &["add", "packages/sub"]);
        git(grit_root, &["add", "packages/sub"]);
        git_at_fixed_time(git_root, &["commit", "-qm", "seed"]);
        git_at_fixed_time(grit_root, &["commit", "-qm", "seed"]);
        fs::write(git_root.join("packages/loose.txt"), b"beside gitlink\n").unwrap();
        fs::write(grit_root.join("packages/loose.txt"), b"beside gitlink\n").unwrap();
    });
}

#[test]
fn fused_add_matches_git_untracked_beside_gitlink() {
    parallel_repos(|git_root, grit_root| {
        init_nested_repo(git_root, "sub");
        init_nested_repo(grit_root, "sub");
        git(git_root, &["add", "sub"]);
        git(grit_root, &["add", "sub"]);
        git_at_fixed_time(git_root, &["commit", "-qm", "seed"]);
        git_at_fixed_time(grit_root, &["commit", "-qm", "seed"]);
        fs::write(git_root.join("sibling.txt"), b"beside gitlink\n").unwrap();
        fs::write(grit_root.join("sibling.txt"), b"beside gitlink\n").unwrap();
    });
}

#[test]
fn fused_add_matches_git_tracked_file_replaced_by_directory() {
    parallel_repos(|git_root, grit_root| {
        fs::write(git_root.join("was-file.txt"), b"old\n").unwrap();
        fs::write(grit_root.join("was-file.txt"), b"old\n").unwrap();
        git(git_root, &["add", "was-file.txt"]);
        git(grit_root, &["add", "was-file.txt"]);
        git_at_fixed_time(git_root, &["commit", "-qm", "seed"]);
        git_at_fixed_time(grit_root, &["commit", "-qm", "seed"]);
        fs::remove_file(git_root.join("was-file.txt")).unwrap();
        fs::remove_file(grit_root.join("was-file.txt")).unwrap();
        fs::create_dir_all(git_root.join("was-file.txt")).unwrap();
        fs::create_dir_all(grit_root.join("was-file.txt")).unwrap();
        fs::write(git_root.join("was-file.txt/child.txt"), b"new\n").unwrap();
        fs::write(grit_root.join("was-file.txt/child.txt"), b"new\n").unwrap();
    });
}

#[test]
#[cfg(unix)]
fn fused_add_matches_git_directory_symlink_not_descended() {
    parallel_repos(|git_root, grit_root| {
        fs::create_dir_all(git_root.join("target/inside")).unwrap();
        fs::create_dir_all(grit_root.join("target/inside")).unwrap();
        fs::write(git_root.join("target/inside/hidden.txt"), b"hidden\n").unwrap();
        fs::write(grit_root.join("target/inside/hidden.txt"), b"hidden\n").unwrap();
        fs::write(git_root.join("tracked.txt"), b"root\n").unwrap();
        fs::write(grit_root.join("tracked.txt"), b"root\n").unwrap();
        std::os::unix::fs::symlink("target", git_root.join("linkdir")).unwrap();
        std::os::unix::fs::symlink("target", grit_root.join("linkdir")).unwrap();
        fs::write(git_root.join("beside.txt"), b"beside\n").unwrap();
        fs::write(grit_root.join("beside.txt"), b"beside\n").unwrap();
        git(git_root, &["add", "tracked.txt"]);
        git(grit_root, &["add", "tracked.txt"]);
        git_at_fixed_time(git_root, &["commit", "-qm", "seed"]);
        git_at_fixed_time(grit_root, &["commit", "-qm", "seed"]);
    });
}
