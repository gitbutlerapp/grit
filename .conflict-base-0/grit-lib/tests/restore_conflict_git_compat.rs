//! `restore_paths` conflict options compared with system `git restore`.

use std::fs;
use std::process::Command;

use grit_lib::diff::merge_file::ConflictStyle;
use grit_lib::porcelain::restore::{restore_paths, ConflictSide, RestoreOptions, RestoreSource};
use grit_lib::repo::Repository;
use grit_test_support::git;

fn git_cmd(repo: &std::path::Path, args: &[&str]) -> String {
    git(repo, args)
}

fn init_repo() -> (tempfile::TempDir, Repository) {
    let dir = tempfile::tempdir().expect("tempdir");
    git_cmd(dir.path(), &["init"]);
    git_cmd(dir.path(), &["config", "user.email", "test@example.com"]);
    git_cmd(dir.path(), &["config", "user.name", "Test"]);
    let root = dir.path();
    let repo = Repository::open(
        &root.join(".git"),
        &grit_lib::config::environment::RepositoryOptions::empty().with_work_tree(root),
    )
    .expect("open");
    (dir, repo)
}

fn git_merge_expect_conflict(dir: &tempfile::TempDir, branch: &str) {
    let out = Command::new("git")
        .current_dir(dir.path())
        .args(["merge", branch])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git merge");
    assert!(
        !out.status.success(),
        "merge should stop with conflicts: {}",
        String::from_utf8_lossy(&out.stdout)
    );
}

fn merge_conflict_repo(dir: &tempfile::TempDir) {
    fs::write(dir.path().join("f.txt"), "line1\nline2\nline3\n").unwrap();
    git_cmd(dir.path(), &["add", "f.txt"]);
    git_cmd(dir.path(), &["commit", "-qm", "base"]);
    git_cmd(dir.path(), &["branch", "side"]);
    git_cmd(dir.path(), &["switch", "side"]);
    fs::write(dir.path().join("f.txt"), "line1\nside\nline3\n").unwrap();
    git_cmd(dir.path(), &["add", "f.txt"]);
    git_cmd(dir.path(), &["commit", "-qm", "side"]);
    git_cmd(dir.path(), &["switch", "master"]);
    fs::write(dir.path().join("f.txt"), "line1\nmaster\nline3\n").unwrap();
    git_cmd(dir.path(), &["add", "f.txt"]);
    git_cmd(dir.path(), &["commit", "-qm", "master"]);
    git_merge_expect_conflict(dir, "side");
}

fn conflict_opts(side: Option<ConflictSide>, remerge: Option<ConflictStyle>) -> RestoreOptions {
    RestoreOptions {
        pathspecs: vec!["f.txt".into()],
        pathspec_sources: vec!["f.txt".into()],
        source: RestoreSource::Index,
        staged: false,
        worktree: true,
        stage: side,
        remerge,
    }
}

fn worktree_bytes(dir: &tempfile::TempDir) -> Vec<u8> {
    worktree_bytes_path(dir, "f.txt")
}

fn worktree_bytes_path(dir: &tempfile::TempDir, path: &str) -> Vec<u8> {
    fs::read(dir.path().join(path)).unwrap_or_default()
}

fn merge_add_add_conflict_repo(dir: &tempfile::TempDir) {
    fs::write(dir.path().join("anchor.txt"), "anchor\n").unwrap();
    git_cmd(dir.path(), &["add", "anchor.txt"]);
    git_cmd(dir.path(), &["commit", "-qm", "base"]);
    git_cmd(dir.path(), &["branch", "side"]);
    git_cmd(dir.path(), &["switch", "side"]);
    fs::write(dir.path().join("f.txt"), "ours\n").unwrap();
    git_cmd(dir.path(), &["add", "f.txt"]);
    git_cmd(dir.path(), &["commit", "-qm", "side"]);
    git_cmd(dir.path(), &["switch", "master"]);
    fs::write(dir.path().join("f.txt"), "theirs\n").unwrap();
    git_cmd(dir.path(), &["add", "f.txt"]);
    git_cmd(dir.path(), &["commit", "-qm", "master"]);
    git_merge_expect_conflict(dir, "side");
    assert!(
        git_cmd(dir.path(), &["ls-files", "-u", "f.txt"])
            .lines()
            .all(|line| !line.contains(" 1\t")),
        "add/add should not have stage 1: {}",
        git_cmd(dir.path(), &["ls-files", "-u", "f.txt"])
    );
}

fn merge_binary_conflict_repo(dir: &tempfile::TempDir) {
    fs::write(dir.path().join("anchor.txt"), "anchor\n").unwrap();
    git_cmd(dir.path(), &["add", "anchor.txt"]);
    git_cmd(dir.path(), &["commit", "-qm", "base"]);
    git_cmd(dir.path(), &["branch", "side"]);
    git_cmd(dir.path(), &["switch", "side"]);
    fs::write(dir.path().join("f.bin"), [0, 1, b'o', b'u', b'r', b's']).unwrap();
    git_cmd(dir.path(), &["add", "f.bin"]);
    git_cmd(dir.path(), &["commit", "-qm", "side"]);
    git_cmd(dir.path(), &["switch", "master"]);
    fs::write(
        dir.path().join("f.bin"),
        [0, 1, b't', b'h', b'e', b'i', b'r', b's'],
    )
    .unwrap();
    git_cmd(dir.path(), &["add", "f.bin"]);
    git_cmd(dir.path(), &["commit", "-qm", "master"]);
    git_merge_expect_conflict(dir, "side");
}

fn conflict_opts_path(
    path: &str,
    side: Option<ConflictSide>,
    remerge: Option<ConflictStyle>,
) -> RestoreOptions {
    RestoreOptions {
        pathspecs: vec![path.into()],
        pathspec_sources: vec![path.into()],
        source: RestoreSource::Index,
        staged: false,
        worktree: true,
        stage: side,
        remerge,
    }
}

fn git_restore(dir: &tempfile::TempDir, args: &[&str]) {
    let mut cmd = Command::new("git");
    cmd.current_dir(dir.path())
        .arg("restore")
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null");
    let status = cmd.status().expect("git restore");
    assert!(status.success(), "git restore failed");
}

fn ls_files_unmerged(dir: &tempfile::TempDir) -> String {
    git_cmd(dir.path(), &["ls-files", "-u", "f.txt"])
}

#[test]
fn restore_ours_matches_git_checkout_ours_bytes() {
    let (dir_git, _repo_git) = init_repo();
    merge_conflict_repo(&dir_git);
    git_restore(&dir_git, &["--ours", "f.txt"]);
    let want = worktree_bytes(&dir_git);

    let (dir, repo) = init_repo();
    merge_conflict_repo(&dir);
    restore_paths(&repo, &conflict_opts(Some(ConflictSide::Ours), None)).expect("restore ours");
    assert_eq!(worktree_bytes(&dir), want);
}

#[test]
fn restore_theirs_matches_git() {
    let (dir_git, _repo_git) = init_repo();
    merge_conflict_repo(&dir_git);
    git_restore(&dir_git, &["--theirs", "f.txt"]);
    let want = worktree_bytes(&dir_git);

    let (dir, repo) = init_repo();
    merge_conflict_repo(&dir);
    restore_paths(&repo, &conflict_opts(Some(ConflictSide::Theirs), None)).expect("restore theirs");
    assert_eq!(worktree_bytes(&dir), want);
}

#[test]
fn restore_ours_leaves_index_unmerged() {
    let (dir, repo) = init_repo();
    merge_conflict_repo(&dir);
    let before = ls_files_unmerged(&dir);
    restore_paths(&repo, &conflict_opts(Some(ConflictSide::Ours), None)).expect("restore");
    assert_eq!(ls_files_unmerged(&dir), before);
}

#[test]
fn restore_merge_add_add_byte_equal_git() {
    let (dir_git, _repo_git) = init_repo();
    merge_add_add_conflict_repo(&dir_git);
    git_restore(&dir_git, &["--merge", "f.txt"]);
    let want = worktree_bytes(&dir_git);

    let (dir, repo) = init_repo();
    merge_add_add_conflict_repo(&dir);
    restore_paths(
        &repo,
        &conflict_opts_path("f.txt", None, Some(ConflictStyle::Merge)),
    )
    .expect("restore merge add/add");
    assert_eq!(worktree_bytes(&dir), want);
}

#[test]
fn restore_merge_binary_byte_equal_git() {
    let (dir_git, _repo_git) = init_repo();
    merge_binary_conflict_repo(&dir_git);
    git_restore(&dir_git, &["--merge", "f.bin"]);
    let want = worktree_bytes_path(&dir_git, "f.bin");

    let (dir, repo) = init_repo();
    merge_binary_conflict_repo(&dir);
    restore_paths(
        &repo,
        &conflict_opts_path("f.bin", None, Some(ConflictStyle::Merge)),
    )
    .expect("restore merge binary");
    assert_eq!(worktree_bytes_path(&dir, "f.bin"), want);
}

#[test]
fn restore_merge_recreates_markers_byte_equal_git_checkout_m() {
    let (dir_git, _repo_git) = init_repo();
    merge_conflict_repo(&dir_git);
    git_restore(&dir_git, &["--merge", "f.txt"]);
    let want = worktree_bytes(&dir_git);

    let (dir, repo) = init_repo();
    merge_conflict_repo(&dir);
    restore_paths(&repo, &conflict_opts(None, Some(ConflictStyle::Merge))).expect("restore merge");
    assert_eq!(worktree_bytes(&dir), want);
}

#[test]
fn restore_conflict_diff3_byte_equal_git() {
    let (dir_git, _repo_git) = init_repo();
    merge_conflict_repo(&dir_git);
    git_restore(&dir_git, &["--merge", "--conflict=diff3", "f.txt"]);
    let want = worktree_bytes(&dir_git);

    let (dir, repo) = init_repo();
    merge_conflict_repo(&dir);
    restore_paths(&repo, &conflict_opts(None, Some(ConflictStyle::Diff3))).expect("restore diff3");
    assert_eq!(worktree_bytes(&dir), want);
}

#[test]
fn restore_ours_missing_stage_errors() {
    let (dir, repo) = init_repo();
    merge_conflict_repo(&dir);
    let mut index = repo.index_handle().load().expect("index");
    index
        .entries_mut()
        .retain(|e| !(e.path == b"f.txt" && e.stage() == 2));
    repo.index_handle().write(&mut index).expect("write index");
    let err = restore_paths(&repo, &conflict_opts(Some(ConflictSide::Ours), None)).unwrap_err();
    assert!(matches!(
        err,
        grit_lib::error::Error::RestoreConflictMissingOurs { .. }
    ));
}

#[test]
fn restore_ours_with_staged_rejected() {
    let (dir, repo) = init_repo();
    merge_conflict_repo(&dir);
    let err = restore_paths(
        &repo,
        &RestoreOptions {
            pathspecs: vec!["f.txt".into()],
            pathspec_sources: vec!["f.txt".into()],
            source: RestoreSource::Index,
            staged: true,
            worktree: false,
            stage: Some(ConflictSide::Ours),
            remerge: None,
        },
    )
    .unwrap_err();
    assert!(matches!(
        err,
        grit_lib::error::Error::RestoreConflictWithStaged
    ));
}

#[test]
fn restore_merge_uses_resolve_undo() {
    let (dir_git, _repo_git) = init_repo();
    merge_conflict_repo(&dir_git);
    fs::write(dir_git.path().join("f.txt"), "line1\nresolved\nline3\n").unwrap();
    git_cmd(dir_git.path(), &["add", "f.txt"]);
    git_restore(&dir_git, &["--merge", "f.txt"]);
    let want = worktree_bytes(&dir_git);

    let (dir, repo) = init_repo();
    merge_conflict_repo(&dir);
    fs::write(dir.path().join("f.txt"), "line1\nresolved\nline3\n").unwrap();
    git_cmd(dir.path(), &["add", "f.txt"]);
    restore_paths(&repo, &conflict_opts(None, Some(ConflictStyle::Merge)))
        .expect("restore merge after resolve");
    assert_eq!(worktree_bytes(&dir), want);
    assert!(!git_cmd(dir.path(), &["ls-files", "-u"]).trim().is_empty());
}
