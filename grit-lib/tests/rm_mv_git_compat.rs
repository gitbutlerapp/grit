//! `remove_paths` / `move_path` compatibility with the system `git` binary.

use std::fs;
use std::path::Path;
use std::process::Command;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::index::write_tree::{
    cache_tree_fully_valid, write_tree_update_index, WriteTreeFlags,
};
use grit_lib::porcelain::paths::{move_path, remove_paths, RemoveBlockReason, RemoveOptions};
use grit_lib::repo::Repository;
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
    Repository::discover(Some(dir), &RepositoryOptions::empty()).expect("open repo")
}

fn worktree_has_exact_dir_entry(parent: &Path, name: &str) -> bool {
    fs::read_dir(parent)
        .into_iter()
        .flatten()
        .flatten()
        .any(|entry| entry.file_name().to_string_lossy() == name)
}

fn is_case_insensitive(dir: &Path) -> bool {
    for attempt in 0..128_u32 {
        let name = format!(".grit-icase-test-probe-{attempt:05}");
        let lower = dir.join(&name);
        if lower.exists() {
            continue;
        }
        if std::fs::write(&lower, b"").is_err() {
            return false;
        }
        let upper: String = name.chars().map(|c| c.to_ascii_uppercase()).collect();
        let folded = dir.join(upper).exists();
        let _ = std::fs::remove_file(&lower);
        return folded;
    }
    false
}

fn hold_index_lock(repo: &Repository) -> std::fs::File {
    let lock_path = repo.index_handle().path().with_extension("lock");
    std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&lock_path)
        .expect("create index.lock")
}

#[test]
fn rm_leaves_worktree_when_index_lock_held() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo_dir = tmp.path();
    git(repo_dir, &["init", "-q", "-b", "main"]);
    std::fs::write(repo_dir.join("a"), "keep\n").unwrap();
    commit_all(repo_dir, "init");

    let repo = grit_repo(repo_dir);
    let _lock = hold_index_lock(&repo);
    let err = remove_paths(
        &repo,
        &RemoveOptions {
            pathspecs: vec!["a".into()],
            pathspec_sources: vec!["a".into()],
            ..Default::default()
        },
    )
    .expect_err("rm with index.lock");
    assert!(
        matches!(&err, grit_lib::error::Error::LockHeld { path, .. } if path.ends_with(".git/index.lock")),
        "expected index lock failure, got {err:?}"
    );

    assert!(repo_dir.join("a").is_file());
    assert!(
        git_porcelain(repo_dir).trim().is_empty(),
        "index and worktree must be unchanged"
    );
}

#[test]
fn mv_leaves_worktree_when_index_lock_held() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo_dir = tmp.path();
    git(repo_dir, &["init", "-q", "-b", "main"]);
    std::fs::write(repo_dir.join("a"), "payload\n").unwrap();
    commit_all(repo_dir, "init");

    let repo = grit_repo(repo_dir);
    let _lock = hold_index_lock(&repo);
    let err = move_path(&repo, "a", "b", false).expect_err("mv with index.lock");
    assert!(
        matches!(&err, grit_lib::error::Error::LockHeld { path, .. } if path.ends_with(".git/index.lock")),
        "expected index lock failure, got {err:?}"
    );

    assert!(repo_dir.join("a").is_file());
    assert!(!repo_dir.join("b").exists());
    assert!(
        git_porcelain(repo_dir).trim().is_empty(),
        "index and worktree must be unchanged"
    );
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
    assert!(matches!(err, grit_lib::error::Error::RemoveBlocked { .. }));
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
fn mv_leaves_empty_source_directories() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo_dir = tmp.path();
    git(repo_dir, &["init", "-q", "-b", "main"]);
    fs::create_dir_all(repo_dir.join("a/b")).unwrap();
    fs::write(repo_dir.join("a/b/y"), "3\n").unwrap();
    fs::create_dir_all(repo_dir.join("d")).unwrap();
    fs::write(repo_dir.join("d/q"), "4\n").unwrap();
    commit_all(repo_dir, "init");

    let git_dir = tempfile::tempdir().expect("git twin");
    let git_repo_dir = git_dir.path();
    git(git_repo_dir, &["init", "-q", "-b", "main"]);
    fs::create_dir_all(git_repo_dir.join("a/b")).unwrap();
    fs::write(git_repo_dir.join("a/b/y"), "3\n").unwrap();
    fs::create_dir_all(git_repo_dir.join("d")).unwrap();
    fs::write(git_repo_dir.join("d/q"), "4\n").unwrap();
    commit_all(git_repo_dir, "init");

    let repo = grit_repo(repo_dir);
    move_path(&repo, "a/b/y", "a/y2", false).expect("mv a/b/y -> a/y2");
    move_path(&repo, "d/q", "a/q", false).expect("mv d/q -> a/q");

    git(git_repo_dir, &["mv", "a/b/y", "a/y2"]);
    git(git_repo_dir, &["mv", "d/q", "a/q"]);

    assert!(
        repo_dir.join("a/b").is_dir(),
        "emptied a/b must remain after moving its only file"
    );
    assert!(
        repo_dir.join("d").is_dir(),
        "emptied d must remain after moving its only file"
    );
    assert_eq!(
        fs::read_to_string(repo_dir.join("a/y2")).unwrap(),
        "3\n",
        "moved file contents"
    );
    assert_eq!(
        fs::read_to_string(repo_dir.join("a/q")).unwrap(),
        "4\n",
        "moved file contents"
    );
    assert_eq!(
        git_porcelain(repo_dir),
        git_porcelain(git_repo_dir),
        "staged status must match git mv twin"
    );

    fs::create_dir_all(repo_dir.join("src/lib")).unwrap();
    fs::write(repo_dir.join("src/lib/mod.rs"), "mod\n").unwrap();
    commit_all(repo_dir, "add src tree");
    move_path(&repo, "src/lib", "vendor/lib", false).expect("mv src/lib -> vendor/lib");
    assert!(
        repo_dir.join("src").is_dir(),
        "src must remain when only src/lib was moved out"
    );
    assert!(repo_dir.join("vendor/lib/mod.rs").is_file());
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
fn mv_self_move_with_force_is_non_destructive() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo_dir = tmp.path();
    git(repo_dir, &["init", "-q", "-b", "main"]);
    std::fs::write(repo_dir.join("a"), "payload\n").unwrap();
    commit_all(repo_dir, "init");

    let repo = grit_repo(repo_dir);
    let err = move_path(&repo, "a", "a", true).expect_err("self mv");
    assert!(matches!(err, grit_lib::error::Error::MoveIntoSelf { .. }));

    assert!(repo_dir.join("a").is_file(), "worktree file must remain");
    let status = git_porcelain(repo_dir);
    assert!(
        status.trim().is_empty(),
        "index must stay clean after refused self-move, got:\n{status}"
    );
}

#[test]
fn mv_directory_refuses_missing_tracked_child() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo_dir = tmp.path();
    git(repo_dir, &["init", "-q", "-b", "main"]);
    std::fs::create_dir_all(repo_dir.join("d")).unwrap();
    std::fs::write(repo_dir.join("d/a"), "a\n").unwrap();
    std::fs::write(repo_dir.join("d/b"), "b\n").unwrap();
    commit_all(repo_dir, "init");
    std::fs::remove_file(repo_dir.join("d/b")).unwrap();

    let repo = grit_repo(repo_dir);
    let err = move_path(&repo, "d", "e", false).expect_err("mv missing child");
    assert!(matches!(err, grit_lib::error::Error::MoveBadSource { .. }));

    assert!(repo_dir.join("d/a").is_file());
    assert!(!repo_dir.join("d/b").exists());
    assert!(!repo_dir.join("e").exists());
    let status = git_porcelain(repo_dir);
    assert!(
        status.contains(" D d/b") || status.contains("D d/b"),
        "deletion of d/b unchanged:\n{status}"
    );
}

#[test]
fn rm_directory_pathspec_refused_without_recursive() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo_dir = tmp.path();
    git(repo_dir, &["init", "-q", "-b", "main"]);
    std::fs::create_dir_all(repo_dir.join("dir")).unwrap();
    std::fs::write(repo_dir.join("dir/file"), "f\n").unwrap();
    commit_all(repo_dir, "init");
    std::fs::rename(repo_dir.join("dir"), repo_dir.join("dir_hidden")).unwrap();

    let repo = grit_repo(repo_dir);
    let err = remove_paths(
        &repo,
        &RemoveOptions {
            pathspecs: vec!["dir".into()],
            pathspec_sources: vec!["dir".into()],
            recursive: false,
            ..Default::default()
        },
    )
    .expect_err("rm dir without -r");
    assert!(matches!(
        err,
        grit_lib::error::Error::NotRemovingRecursively(_)
    ));
    assert!(repo_dir.join("dir_hidden/file").is_file());
}

#[test]
fn mv_rejects_destination_outside_work_tree() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo_dir = tmp.path();
    git(repo_dir, &["init", "-q", "-b", "main"]);
    std::fs::write(repo_dir.join("tracked"), "body\n").unwrap();
    commit_all(repo_dir, "init");

    let repo = grit_repo(repo_dir);
    let err = move_path(&repo, "tracked", "../escaped", false).expect_err("mv outside repo");
    assert!(matches!(err, grit_lib::error::Error::InvalidPath(_)));
    assert!(repo_dir.join("tracked").is_file());
    let index = repo.index_handle().load().expect("index");
    assert!(index.get("tracked".as_bytes(), 0).is_some());
}

#[test]
fn mv_refused_when_index_lock_held_externally() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo_dir = tmp.path();
    git(repo_dir, &["init", "-q", "-b", "main"]);
    std::fs::write(repo_dir.join("tracked"), "body\n").unwrap();
    commit_all(repo_dir, "init");

    let lock_path = repo_dir.join(".git/index.lock");
    fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&lock_path)
        .expect("pre-create index.lock");

    let repo = grit_repo(repo_dir);
    let index_before = repo.index_handle().load().expect("index before");
    let err = move_path(&repo, "tracked", "moved", false).expect_err("mv with held lock");
    assert!(
        matches!(&err, grit_lib::error::Error::LockHeld { path, .. } if path.ends_with(".git/index.lock")),
        "expected index lock failure, got {err:?}"
    );
    assert!(repo_dir.join("tracked").is_file());
    assert!(!repo_dir.join("moved").exists());
    assert!(git_porcelain(repo_dir).is_empty());
    assert_eq!(
        index_before.entries(),
        repo.index_handle().load().expect("index after").entries()
    );
}

#[test]
fn mv_worktree_failure_leaves_index_unchanged() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo_dir = tmp.path();
    git(repo_dir, &["init", "-q", "-b", "main"]);
    std::fs::write(repo_dir.join("tracked"), "body\n").unwrap();
    std::fs::write(repo_dir.join("blocked"), "blocker\n").unwrap();
    commit_all(repo_dir, "init");

    let repo = grit_repo(repo_dir);
    let index_before = repo.index_handle().load().expect("index before");
    let err = move_path(&repo, "tracked", "blocked/child", false).expect_err("mv into file");
    assert!(matches!(err, grit_lib::error::Error::PathError(_)));
    assert!(repo_dir.join("tracked").is_file());
    assert!(repo_dir.join("blocked").is_file());
    assert_eq!(
        index_before.entries(),
        repo.index_handle().load().expect("index after").entries()
    );
}

#[test]
fn mv_force_refused_when_index_lock_held_externally() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo_dir = tmp.path();
    git(repo_dir, &["init", "-q", "-b", "main"]);
    std::fs::write(repo_dir.join("source"), "from\n").unwrap();
    std::fs::write(repo_dir.join("destination"), "to\n").unwrap();
    commit_all(repo_dir, "init");

    let lock_path = repo_dir.join(".git/index.lock");
    fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&lock_path)
        .expect("pre-create index.lock");

    let repo = grit_repo(repo_dir);
    let index_before = repo.index_handle().load().expect("index before");
    let err = move_path(&repo, "source", "destination", true).expect_err("force mv with lock");
    assert!(
        matches!(&err, grit_lib::error::Error::LockHeld { path, .. } if path.ends_with(".git/index.lock")),
        "expected index lock failure, got {err:?}"
    );
    assert!(repo_dir.join("source").is_file());
    assert!(repo_dir.join("destination").is_file());
    let status = git_porcelain(repo_dir);
    assert!(
        status.is_empty(),
        "force mv must not delete destination when lock is held:\n{status}"
    );
    let index_after = repo.index_handle().load().expect("index after");
    assert_eq!(index_before.entries(), index_after.entries());
    assert!(index_after.get("source".as_bytes(), 0).is_some());
    assert!(index_after.get("destination".as_bytes(), 0).is_some());
}

#[test]
fn mv_rejects_in_repo_absolute_destination() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo_dir = tmp.path();
    git(repo_dir, &["init", "-q", "-b", "main"]);
    std::fs::write(repo_dir.join("source"), "payload\n").unwrap();
    std::fs::write(repo_dir.join("destination"), "dest\n").unwrap();
    commit_all(repo_dir, "init");

    let repo = grit_repo(repo_dir);
    let abs_dst = repo_dir.join("destination");
    let err = move_path(&repo, "source", abs_dst.to_str().expect("utf8 path"), false)
        .expect_err("absolute destination");
    assert!(matches!(err, grit_lib::error::Error::InvalidPath(_)));

    assert!(repo_dir.join("source").is_file());
    assert!(repo_dir.join("destination").is_file());
    assert!(
        git_porcelain(repo_dir).trim().is_empty(),
        "index must be unchanged:\n{}",
        git_porcelain(repo_dir)
    );
}

#[test]
fn mv_rejects_parent_component_in_source() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo_dir = tmp.path();
    git(repo_dir, &["init", "-q", "-b", "main"]);
    std::fs::create_dir_all(repo_dir.join("dir")).unwrap();
    std::fs::write(repo_dir.join("source"), "payload\n").unwrap();
    commit_all(repo_dir, "init");

    let repo = grit_repo(repo_dir);
    let err = move_path(&repo, "dir/../source", "moved", false).expect_err(".. source");
    assert!(matches!(err, grit_lib::error::Error::InvalidPath(_)));

    assert!(repo_dir.join("source").is_file());
    assert!(!repo_dir.join("moved").exists());
    assert!(
        git_porcelain(repo_dir).trim().is_empty(),
        "index must be unchanged:\n{}",
        git_porcelain(repo_dir)
    );
}

#[test]
#[cfg(unix)]
fn mv_force_failure_restores_destination_and_index() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = tempfile::tempdir().expect("tempdir");
    let repo_dir = tmp.path();
    git(repo_dir, &["init", "-q", "-b", "main"]);
    std::fs::create_dir_all(repo_dir.join("locked")).unwrap();
    std::fs::write(repo_dir.join("locked/source"), "from\n").unwrap();
    std::fs::write(repo_dir.join("destination"), "to\n").unwrap();
    commit_all(repo_dir, "init");

    let mut perms = std::fs::metadata(repo_dir.join("locked"))
        .expect("locked meta")
        .permissions();
    perms.set_mode(0o555);
    std::fs::set_permissions(repo_dir.join("locked"), perms).expect("chmod locked");

    let repo = grit_repo(repo_dir);
    let err = move_path(&repo, "locked/source", "destination", true).expect_err("forced mv");
    assert!(
        matches!(err, grit_lib::error::Error::PathError(_)),
        "unexpected error: {err:?}"
    );

    assert!(repo_dir.join("locked/source").is_file());
    assert!(repo_dir.join("destination").is_file());
    assert!(
        git_porcelain(repo_dir).trim().is_empty(),
        "index and worktree must match pre-mv state:\n{}",
        git_porcelain(repo_dir)
    );
}

#[test]
#[cfg(unix)]
fn mv_force_failure_restores_executable_destination_mode() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = tempfile::tempdir().expect("tempdir");
    let repo_dir = tmp.path();
    git(repo_dir, &["init", "-q", "-b", "main"]);
    std::fs::create_dir_all(repo_dir.join("locked")).unwrap();
    std::fs::write(repo_dir.join("locked/source"), "from\n").unwrap();
    std::fs::write(repo_dir.join("destination"), "#!/bin/sh\n").unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        let mut dest_mode = std::fs::metadata(repo_dir.join("destination"))
            .expect("destination meta")
            .permissions();
        dest_mode.set_mode(0o755);
        std::fs::set_permissions(repo_dir.join("destination"), dest_mode)
            .expect("chmod destination");
    }
    commit_all(repo_dir, "init");

    let mut locked_mode = std::fs::metadata(repo_dir.join("locked"))
        .expect("locked meta")
        .permissions();
    locked_mode.set_mode(0o555);
    std::fs::set_permissions(repo_dir.join("locked"), locked_mode).expect("chmod locked");

    let repo = grit_repo(repo_dir);
    move_path(&repo, "locked/source", "destination", true).expect_err("forced mv");

    let mode = std::fs::metadata(repo_dir.join("destination"))
        .expect("destination meta")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o755, "executable bit must be preserved on rollback");
    assert!(
        git_porcelain(repo_dir).trim().is_empty(),
        "worktree must be clean after rollback:\n{}",
        git_porcelain(repo_dir)
    );
}

#[test]
#[cfg(unix)]
fn mv_force_partial_destination_cleanup_restores_removed_files() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = tempfile::tempdir().expect("tempdir");
    let repo_dir = tmp.path();
    git(repo_dir, &["init", "-q", "-b", "main"]);
    std::fs::create_dir_all(repo_dir.join("source")).unwrap();
    std::fs::write(repo_dir.join("source/root.txt"), "root\n").unwrap();
    std::fs::create_dir_all(repo_dir.join("target/source")).unwrap();
    std::fs::write(repo_dir.join("target/source/a"), "a\n").unwrap();
    std::fs::create_dir_all(repo_dir.join("target/source/locked")).unwrap();
    std::fs::write(repo_dir.join("target/source/locked/b"), "b\n").unwrap();
    commit_all(repo_dir, "init");

    let mut locked_mode = std::fs::metadata(repo_dir.join("target/source/locked"))
        .expect("locked meta")
        .permissions();
    locked_mode.set_mode(0o555);
    std::fs::set_permissions(repo_dir.join("target/source/locked"), locked_mode)
        .expect("chmod locked");

    let repo = grit_repo(repo_dir);
    move_path(&repo, "source", "target", true).expect_err("forced mv into occupied tree");

    assert!(repo_dir.join("target/source/a").is_file());
    assert!(repo_dir.join("target/source/locked/b").is_file());
    assert!(repo_dir.join("source/root.txt").is_file());
    assert!(
        git_porcelain(repo_dir).trim().is_empty(),
        "index and worktree must be unchanged:\n{}",
        git_porcelain(repo_dir)
    );
}

#[test]
fn mv_case_only_does_not_clobber_predictable_temp_pattern_file() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo_dir = tmp.path();
    git(repo_dir, &["init", "-q", "-b", "main"]);
    git(repo_dir, &["config", "core.ignorecase", "true"]);
    fs::write(repo_dir.join("README.md"), "hi\n").unwrap();
    let decoy = format!(".readme.md.grit-mv-{}", std::process::id());
    fs::write(repo_dir.join(&decoy), "keep\n").unwrap();
    commit_all(repo_dir, "init");

    let repo = grit_repo(repo_dir);
    move_path(&repo, "README.md", "readme.md", false).expect("case-only mv");

    assert!(
        repo_dir.join(&decoy).is_file(),
        "unrelated file matching the old temp pattern must survive case-only mv"
    );
    let index = repo.index_handle().load().expect("index");
    assert!(
        index.get(decoy.as_bytes(), 0).is_some(),
        "index entry for decoy must remain"
    );
    assert!(worktree_has_exact_dir_entry(repo_dir, "readme.md"));
}

#[test]
fn mv_case_only_directory_with_ignorecase() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo_dir = tmp.path();
    git(repo_dir, &["init", "-q", "-b", "main"]);
    git(repo_dir, &["config", "core.ignorecase", "true"]);
    fs::create_dir(repo_dir.join("Docs")).unwrap();
    fs::write(repo_dir.join("Docs/a.txt"), "in dir\n").unwrap();
    commit_all(repo_dir, "init");

    let repo = grit_repo(repo_dir);
    move_path(&repo, "Docs", "docs", false).expect("case-only directory mv");

    assert!(worktree_has_exact_dir_entry(repo_dir, "docs"));
    assert!(!worktree_has_exact_dir_entry(repo_dir, "Docs"));
    assert!(repo_dir.join("docs/a.txt").is_file());

    let index = repo.index_handle().load().expect("index");
    let entry = index
        .get(b"docs/a.txt", 0)
        .expect("index tracks renamed tree path");
    assert_eq!(String::from_utf8_lossy(&entry.path), "docs/a.txt");

    if Command::new("git").output().is_ok() {
        let ls = Command::new("git")
            .current_dir(repo_dir)
            .args(["ls-files"])
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .expect("git ls-files");
        assert!(ls.status.success());
        assert_eq!(String::from_utf8_lossy(&ls.stdout).trim(), "docs/a.txt");
    }
}

/// Issue #2376: on a folding volume `Dir` and `dir` are the same dentry; grit must not
/// plan `dir/Dir`.
#[test]
fn mv_case_only_directory_on_folding_volume() {
    let tmp = tempfile::tempdir().expect("tempdir");
    if !is_case_insensitive(tmp.path()) {
        return;
    }
    let repo_dir = tmp.path();
    git(repo_dir, &["init", "-q", "-b", "main"]);
    git(repo_dir, &["config", "core.ignorecase", "true"]);
    fs::create_dir(repo_dir.join("Dir")).unwrap();
    fs::write(repo_dir.join("Dir/x.txt"), "d\n").unwrap();
    commit_all(repo_dir, "one");

    let repo = grit_repo(repo_dir);
    move_path(&repo, "Dir", "dir", false).expect("case-only directory mv on folding volume");

    assert!(worktree_has_exact_dir_entry(repo_dir, "dir"));
    assert!(!worktree_has_exact_dir_entry(repo_dir, "Dir"));
    assert!(repo_dir.join("dir/x.txt").is_file());

    let index = repo.index_handle().load().expect("index");
    let entry = index
        .get(b"dir/x.txt", 0)
        .expect("index tracks renamed tree path");
    assert_eq!(String::from_utf8_lossy(&entry.path), "dir/x.txt");

    if Command::new("git").output().is_ok() {
        let ls = Command::new("git")
            .current_dir(repo_dir)
            .args(["ls-files"])
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .expect("git ls-files");
        assert!(ls.status.success());
        assert_eq!(String::from_utf8_lossy(&ls.stdout).trim(), "dir/x.txt");
        assert!(
            git_diff_cached_rename(repo_dir),
            "git diff --cached -M should detect the staged rename"
        );
        fsck_strict(repo_dir);
    }
}

#[test]
fn mv_case_only_rename_with_ignorecase() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo_dir = tmp.path();
    git(repo_dir, &["init", "-q", "-b", "main"]);
    git(repo_dir, &["config", "core.ignorecase", "true"]);
    fs::write(repo_dir.join("README.md"), "hi\n").unwrap();
    commit_all(repo_dir, "init");

    let repo = grit_repo(repo_dir);
    move_path(&repo, "README.md", "readme.md", false).expect("case-only mv");

    assert!(worktree_has_exact_dir_entry(repo_dir, "readme.md"));
    assert!(!worktree_has_exact_dir_entry(repo_dir, "README.md"));

    let index = repo.index_handle().load().expect("index");
    let entry = index
        .get(b"readme.md", 0)
        .expect("index tracks destination spelling");
    assert_eq!(String::from_utf8_lossy(&entry.path), "readme.md");

    if Command::new("git").output().is_ok() {
        let ls = Command::new("git")
            .current_dir(repo_dir)
            .args(["ls-files"])
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .expect("git ls-files");
        assert!(ls.status.success());
        assert_eq!(String::from_utf8_lossy(&ls.stdout).trim(), "readme.md");
        let status = git_porcelain(repo_dir);
        assert!(
            git_diff_cached_rename(repo_dir) || status.contains("readme.md"),
            "git should see a staged rename after case-only mv:\n{status}"
        );
    }
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
    assert!(matches!(
        err,
        grit_lib::error::Error::MoveDestinationExists { .. }
    ));
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

    let mut index = repo.index_handle().load().expect("index");
    let _tree = write_tree_update_index(repo.odb(), &mut index, "", WriteTreeFlags::default())
        .expect("write-tree");
    assert!(cache_tree_fully_valid(
        repo.odb(),
        index.cache_tree.as_ref()
    ));
    repo.index_handle().write(&mut index).expect("write index");

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

fn rm_opts(path: &str, cached: bool) -> RemoveOptions {
    RemoveOptions {
        pathspecs: vec![path.into()],
        pathspec_sources: vec![path.into()],
        cached,
        ..Default::default()
    }
}

fn blocked_reasons(err: grit_lib::error::Error) -> Vec<(String, RemoveBlockReason)> {
    match err {
        grit_lib::error::Error::RemoveBlocked { paths } => {
            paths.into_iter().map(|b| (b.path, b.reason)).collect()
        }
        other => panic!("expected RemoveBlocked, got {other:?}"),
    }
}

#[test]
fn rm_cached_keeps_committed_file_edited_since() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo_dir = tmp.path();
    git(repo_dir, &["init", "-q", "-b", "main"]);
    fs::write(repo_dir.join(".env"), "secret\n").unwrap();
    commit_all(repo_dir, "oops");
    fs::write(repo_dir.join(".env"), "secret\nmore\n").unwrap();

    let repo = grit_repo(repo_dir);
    let outcome = remove_paths(&repo, &rm_opts(".env", true)).expect("rm --cached");
    assert_eq!(outcome.removed, vec![".env".to_string()]);

    assert_eq!(
        fs::read_to_string(repo_dir.join(".env")).unwrap(),
        "secret\nmore\n",
        "working copy must keep the edits"
    );
    let status = git_porcelain(repo_dir);
    assert!(
        status.contains("D  .env") && status.contains("?? .env"),
        "expected staged deletion plus untracked file, got:\n{status}"
    );
    fsck_strict(repo_dir);
}

#[test]
fn rm_cached_refuses_staged_then_edited_file() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo_dir = tmp.path();
    git(repo_dir, &["init", "-q", "-b", "main"]);
    fs::write(repo_dir.join("f.txt"), "v1\n").unwrap();
    commit_all(repo_dir, "init");
    fs::write(repo_dir.join("f.txt"), "v2\n").unwrap();
    git(repo_dir, &["add", "f.txt"]);
    fs::write(repo_dir.join("f.txt"), "v3\n").unwrap();
    let status_before = git_porcelain(repo_dir);

    let repo = grit_repo(repo_dir);
    let err = remove_paths(&repo, &rm_opts("f.txt", true)).expect_err("must refuse");
    assert_eq!(
        blocked_reasons(err),
        vec![("f.txt".to_string(), RemoveBlockReason::StagedAndLocal)]
    );
    assert_eq!(git_porcelain(repo_dir), status_before, "index untouched");

    let err = remove_paths(&repo, &rm_opts("f.txt", false)).expect_err("must refuse");
    assert_eq!(
        blocked_reasons(err),
        vec![("f.txt".to_string(), RemoveBlockReason::StagedAndLocal)]
    );
}

#[test]
fn rm_cached_untracks_freshly_added_file() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo_dir = tmp.path();
    git(repo_dir, &["init", "-q", "-b", "main"]);
    fs::write(repo_dir.join("base.txt"), "base\n").unwrap();
    commit_all(repo_dir, "init");
    fs::write(repo_dir.join("new.txt"), "new\n").unwrap();
    git(repo_dir, &["add", "new.txt"]);

    let repo = grit_repo(repo_dir);
    let err = remove_paths(&repo, &rm_opts("new.txt", false)).expect_err("staged, no --cached");
    assert_eq!(
        blocked_reasons(err),
        vec![("new.txt".to_string(), RemoveBlockReason::StagedChanges)]
    );

    remove_paths(&repo, &rm_opts("new.txt", true)).expect("rm --cached fresh file");
    assert!(repo_dir.join("new.txt").is_file());
    assert_eq!(git_porcelain(repo_dir), "?? new.txt\n");
}

#[test]
fn rm_cached_untracks_file_before_first_commit() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo_dir = tmp.path();
    git(repo_dir, &["init", "-q", "-b", "main"]);
    fs::write(repo_dir.join("a.txt"), "a\n").unwrap();
    git(repo_dir, &["add", "a.txt"]);

    let repo = grit_repo(repo_dir);
    let err = remove_paths(&repo, &rm_opts("a.txt", false)).expect_err("unborn HEAD");
    assert_eq!(
        blocked_reasons(err),
        vec![("a.txt".to_string(), RemoveBlockReason::StagedChanges)]
    );
    remove_paths(&repo, &rm_opts("a.txt", true)).expect("rm --cached unborn");
    assert!(repo_dir.join("a.txt").is_file());
    assert_eq!(git_porcelain(repo_dir), "?? a.txt\n");
}

#[test]
fn rm_staged_change_refused_without_cached() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo_dir = tmp.path();
    git(repo_dir, &["init", "-q", "-b", "main"]);
    fs::write(repo_dir.join("s.txt"), "v1\n").unwrap();
    commit_all(repo_dir, "init");
    fs::write(repo_dir.join("s.txt"), "v2\n").unwrap();
    git(repo_dir, &["add", "s.txt"]);

    let repo = grit_repo(repo_dir);
    let err = remove_paths(&repo, &rm_opts("s.txt", false)).expect_err("staged change");
    assert_eq!(
        blocked_reasons(err),
        vec![("s.txt".to_string(), RemoveBlockReason::StagedChanges)]
    );
    assert!(repo_dir.join("s.txt").is_file());

    remove_paths(&repo, &rm_opts("s.txt", true)).expect("index matches worktree");
    assert_eq!(fs::read_to_string(repo_dir.join("s.txt")).unwrap(), "v2\n");
    let status = git_porcelain(repo_dir);
    assert!(
        status.contains("D  s.txt") && status.contains("?? s.txt"),
        "got:\n{status}"
    );
}

#[test]
fn rm_modified_reports_local_modifications() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo_dir = tmp.path();
    git(repo_dir, &["init", "-q", "-b", "main"]);
    fs::write(repo_dir.join("dirty.txt"), "v1\n").unwrap();
    commit_all(repo_dir, "init");
    fs::write(repo_dir.join("dirty.txt"), "v2\n").unwrap();

    let repo = grit_repo(repo_dir);
    let err = remove_paths(&repo, &rm_opts("dirty.txt", false)).expect_err("refuse");
    assert_eq!(
        blocked_reasons(err),
        vec![(
            "dirty.txt".to_string(),
            RemoveBlockReason::LocalModifications
        )]
    );
}

#[test]
fn rm_file_already_deleted_from_worktree() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo_dir = tmp.path();
    git(repo_dir, &["init", "-q", "-b", "main"]);
    fs::write(repo_dir.join("gone.txt"), "v1\n").unwrap();
    fs::write(repo_dir.join("staged.txt"), "v1\n").unwrap();
    commit_all(repo_dir, "init");
    fs::write(repo_dir.join("staged.txt"), "v2\n").unwrap();
    git(repo_dir, &["add", "staged.txt"]);
    fs::remove_file(repo_dir.join("gone.txt")).unwrap();
    fs::remove_file(repo_dir.join("staged.txt")).unwrap();

    let repo = grit_repo(repo_dir);
    let mut opts = rm_opts("gone.txt", false);
    opts.pathspecs.push("staged.txt".into());
    opts.pathspec_sources.push("staged.txt".into());
    let outcome = remove_paths(&repo, &opts).expect("nothing left on disk to lose");
    assert_eq!(outcome.removed, vec!["gone.txt", "staged.txt"]);
    assert_eq!(git_porcelain(repo_dir), "D  gone.txt\nD  staged.txt\n");
    fsck_strict(repo_dir);
}

#[test]
fn rm_refusal_lists_every_blocked_path_with_its_reason() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo_dir = tmp.path();
    git(repo_dir, &["init", "-q", "-b", "main"]);
    for name in ["clean", "local", "staged", "both"] {
        fs::write(repo_dir.join(name), "v1\n").unwrap();
    }
    commit_all(repo_dir, "init");
    fs::write(repo_dir.join("local"), "v2\n").unwrap();
    fs::write(repo_dir.join("staged"), "v2\n").unwrap();
    fs::write(repo_dir.join("both"), "v2\n").unwrap();
    git(repo_dir, &["add", "staged", "both"]);
    fs::write(repo_dir.join("both"), "v3\n").unwrap();

    let repo = grit_repo(repo_dir);
    let mut opts = rm_opts("clean", false);
    for name in ["local", "staged", "both"] {
        opts.pathspecs.push(name.into());
        opts.pathspec_sources.push(name.into());
    }
    let err = remove_paths(&repo, &opts).expect_err("refuse");
    assert_eq!(
        blocked_reasons(err),
        vec![
            ("both".to_string(), RemoveBlockReason::StagedAndLocal),
            ("local".to_string(), RemoveBlockReason::LocalModifications),
            ("staged".to_string(), RemoveBlockReason::StagedChanges),
        ]
    );

    opts.cached = true;
    let err = remove_paths(&repo, &opts).expect_err("both still refused with --cached");
    assert_eq!(
        blocked_reasons(err),
        vec![("both".to_string(), RemoveBlockReason::StagedAndLocal)]
    );

    opts.force = true;
    let outcome = remove_paths(&repo, &opts).expect("--cached --force");
    assert_eq!(outcome.removed.len(), 4);
    for name in ["clean", "local", "staged", "both"] {
        assert!(repo_dir.join(name).is_file(), "{name} kept on disk");
    }
    fsck_strict(repo_dir);
}
