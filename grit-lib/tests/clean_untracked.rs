//! `clean_untracked` compatibility with system `git clean`.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::process::Command;

use grit_lib::porcelain::clean::{clean_untracked, CleanOptions};
use grit_lib::porcelain::commit::{create_commit, CommitRequest};
use grit_lib::progress::NullProgress;
use grit_lib::repo::Repository;
use grit_test_support::git;

fn ident() -> String {
    "Clean Test <clean@example.com> 1700000000 +0000".to_owned()
}

fn commit_req(message: &str) -> CommitRequest {
    let id = ident();
    CommitRequest {
        message: message.to_owned(),
        author: id.clone(),
        committer: id,
        allow_empty: false,
        sign_override: None,
        amend: false,
    }
}

fn git_out(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("spawn git");
    assert!(
        out.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).to_string()
}

fn git_clean_preview(root: &Path, extra: &[&str]) -> BTreeSet<String> {
    let mut args = vec!["clean", "-n", "-d"];
    args.extend_from_slice(extra);
    let stdout = git_out(root, &args);
    parse_git_clean_lines(&stdout)
}

fn git_clean_force(root: &Path, extra: &[&str]) {
    let mut args = vec!["clean", "-f", "-d"];
    args.extend_from_slice(extra);
    git_out(root, &args);
}

fn parse_git_clean_lines(stdout: &str) -> BTreeSet<String> {
    stdout
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            line.strip_prefix("Would remove ")
                .or_else(|| line.strip_prefix("Removing "))
        })
        .map(normalize_clean_path)
        .collect()
}

fn normalize_clean_path(path: &str) -> String {
    path.trim_end_matches('/').replace('\\', "/")
}

fn worktree_file_set(root: &Path) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    walk_files(root, root, &mut out);
    out
}

fn walk_files(root: &Path, dir: &Path, out: &mut BTreeSet<String>) {
    let entries = fs::read_dir(dir).expect("read_dir");
    for entry in entries.filter_map(|e| e.ok()) {
        let name = entry.file_name();
        if name == ".git" {
            continue;
        }
        let path = entry.path();
        if path.is_dir() {
            walk_files(root, &path, out);
        } else {
            let rel = path
                .strip_prefix(root)
                .expect("strip")
                .to_string_lossy()
                .replace('\\', "/");
            out.insert(rel);
        }
    }
}

fn init_tracked_repo(root: &Path) -> Repository {
    git(root, &["init", "-q", "-b", "main", "."]);
    git(root, &["config", "user.email", "clean@example.com"]);
    git(root, &["config", "user.name", "Clean Test"]);
    fs::write(root.join("tracked.txt"), b"tracked\n").unwrap();
    let repo = Repository::discover(Some(root)).expect("open");
    grit_lib::porcelain::add::stage(
        &repo,
        &grit_lib::porcelain::add::StageOptions::default(),
        &mut NullProgress,
    )
    .expect("stage");
    create_commit(&repo, &commit_req("base"), &mut NullProgress).expect("commit");
    repo
}

fn populate_fixture(root: &Path) {
    fs::write(root.join("untracked.txt"), b"u\n").unwrap();
    fs::create_dir_all(root.join("empty_dir")).unwrap();
    fs::create_dir_all(root.join("nested/untracked")).unwrap();
    fs::write(root.join("nested/untracked/file.txt"), b"f\n").unwrap();
    fs::write(root.join(".gitignore"), b"ignored.log\n").unwrap();
    fs::write(root.join("ignored.log"), b"i\n").unwrap();
}

fn open_nested_repo(nested_root: &Path) {
    let git = nested_root.join(".git");
    fs::create_dir_all(git.join("objects")).unwrap();
    fs::create_dir_all(git.join("refs/heads")).unwrap();
    fs::write(git.join("HEAD"), "ref: refs/heads/main\n").unwrap();
    fs::write(
        git.join("config"),
        "[core]\n\trepositoryformatversion = 0\n\tbare = false\n",
    )
    .unwrap();
}

#[test]
fn dry_run_lists_same_as_git_clean_nd() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    init_tracked_repo(root);
    populate_fixture(root);

    let repo = Repository::discover(Some(root)).expect("reopen");
    let before = worktree_file_set(root);
    let outcome = clean_untracked(
        &repo,
        &CleanOptions {
            directories: true,
            dry_run: true,
            ..CleanOptions::default()
        },
        &mut NullProgress,
    )
    .expect("dry run");

    assert_eq!(worktree_file_set(root), before, "dry run must not delete");

    let grit_set: BTreeSet<String> = outcome
        .removed
        .iter()
        .map(|p| normalize_clean_path(p))
        .collect();
    let git_set = git_clean_preview(root, &[]);
    assert_eq!(grit_set, git_set, "preview set must match git clean -nd");
}

#[test]
fn force_removes_same_as_git_clean_fd() {
    let grit_root = tempfile::tempdir().expect("grit");
    init_tracked_repo(grit_root.path());
    populate_fixture(grit_root.path());

    let git_root = tempfile::tempdir().expect("git");
    init_tracked_repo(git_root.path());
    populate_fixture(git_root.path());

    let repo = Repository::discover(Some(grit_root.path())).expect("open");
    clean_untracked(
        &repo,
        &CleanOptions {
            directories: true,
            dry_run: false,
            ..CleanOptions::default()
        },
        &mut NullProgress,
    )
    .expect("force clean");

    git_clean_force(git_root.path(), &[]);

    assert_eq!(
        worktree_file_set(grit_root.path()),
        worktree_file_set(git_root.path()),
        "remaining files must match git clean -fd"
    );
}

#[test]
fn include_ignored_matches_git_clean_fdx() {
    let grit_root = tempfile::tempdir().expect("grit");
    init_tracked_repo(grit_root.path());
    populate_fixture(grit_root.path());

    let git_root = tempfile::tempdir().expect("git");
    init_tracked_repo(git_root.path());
    populate_fixture(git_root.path());

    let repo = Repository::discover(Some(grit_root.path())).expect("open");
    clean_untracked(
        &repo,
        &CleanOptions {
            directories: true,
            include_ignored: true,
            dry_run: false,
            ..CleanOptions::default()
        },
        &mut NullProgress,
    )
    .expect("clean -x");

    git_clean_force(git_root.path(), &["-x"]);

    assert_eq!(
        worktree_file_set(grit_root.path()),
        worktree_file_set(git_root.path()),
        "remaining files must match git clean -fdx"
    );
}

#[test]
fn collapsed_parent_with_nested_repo_preserves_nested_and_matches_git() {
    let grit_root = tempfile::tempdir().expect("grit");
    init_tracked_repo(grit_root.path());
    let container = grit_root.path().join("container");
    fs::create_dir_all(container.join("nested")).unwrap();
    open_nested_repo(&container.join("nested"));
    fs::write(container.join("loose.txt"), b"u\n").unwrap();

    let git_root = tempfile::tempdir().expect("git");
    init_tracked_repo(git_root.path());
    let git_container = git_root.path().join("container");
    fs::create_dir_all(git_container.join("nested")).unwrap();
    open_nested_repo(&git_container.join("nested"));
    fs::write(git_container.join("loose.txt"), b"u\n").unwrap();

    let repo = Repository::discover(Some(grit_root.path())).expect("open");
    let preview = clean_untracked(
        &repo,
        &CleanOptions {
            directories: true,
            dry_run: true,
            ..CleanOptions::default()
        },
        &mut NullProgress,
    )
    .expect("dry run");
    let grit_preview: BTreeSet<String> = preview
        .removed
        .iter()
        .map(|p| normalize_clean_path(p))
        .collect();
    assert_eq!(
        grit_preview,
        git_clean_preview(git_root.path(), &[]),
        "preview must match git clean -nd"
    );

    clean_untracked(
        &repo,
        &CleanOptions {
            directories: true,
            dry_run: false,
            ..CleanOptions::default()
        },
        &mut NullProgress,
    )
    .expect("force clean");

    git_clean_force(git_root.path(), &[]);

    assert!(
        grit_root.path().join("container/nested/.git").exists(),
        "nested repository must survive"
    );
    assert_eq!(
        worktree_file_set(grit_root.path()),
        worktree_file_set(git_root.path())
    );
}

#[test]
fn nested_empty_directories_removed_at_highest_level_like_git() {
    let grit_root = tempfile::tempdir().expect("grit");
    init_tracked_repo(grit_root.path());
    fs::create_dir_all(grit_root.path().join("outer/inner")).unwrap();

    let git_root = tempfile::tempdir().expect("git");
    init_tracked_repo(git_root.path());
    fs::create_dir_all(git_root.path().join("outer/inner")).unwrap();

    let repo = Repository::discover(Some(grit_root.path())).expect("open");
    clean_untracked(
        &repo,
        &CleanOptions {
            directories: true,
            dry_run: false,
            ..CleanOptions::default()
        },
        &mut NullProgress,
    )
    .expect("clean");

    git_clean_force(git_root.path(), &[]);

    assert!(!grit_root.path().join("outer").exists());
    assert!(!git_root.path().join("outer").exists());
    assert_eq!(
        worktree_file_set(grit_root.path()),
        worktree_file_set(git_root.path())
    );
}

#[test]
fn collapsed_dir_preserves_ignored_files_like_git() {
    let grit_root = tempfile::tempdir().expect("grit");
    init_tracked_repo(grit_root.path());
    fs::write(grit_root.path().join(".gitignore"), b"ignored.log\n").unwrap();
    grit_lib::porcelain::add::stage(
        &Repository::discover(Some(grit_root.path())).unwrap(),
        &grit_lib::porcelain::add::StageOptions::default(),
        &mut NullProgress,
    )
    .unwrap();
    create_commit(
        &Repository::discover(Some(grit_root.path())).unwrap(),
        &commit_req("ignore"),
        &mut NullProgress,
    )
    .unwrap();
    fs::create_dir_all(grit_root.path().join("mixed")).unwrap();
    fs::write(grit_root.path().join("mixed/untracked.txt"), b"u\n").unwrap();
    fs::write(grit_root.path().join("mixed/ignored.log"), b"i\n").unwrap();

    let git_root = tempfile::tempdir().expect("git");
    init_tracked_repo(git_root.path());
    fs::write(git_root.path().join(".gitignore"), b"ignored.log\n").unwrap();
    git(git_root.path(), &["add", ".gitignore"]);
    git(git_root.path(), &["commit", "-m", "ignore"]);
    fs::create_dir_all(git_root.path().join("mixed")).unwrap();
    fs::write(git_root.path().join("mixed/untracked.txt"), b"u\n").unwrap();
    fs::write(git_root.path().join("mixed/ignored.log"), b"i\n").unwrap();

    let repo = Repository::discover(Some(grit_root.path())).expect("open");
    let preview = clean_untracked(
        &repo,
        &CleanOptions {
            directories: true,
            dry_run: true,
            ..CleanOptions::default()
        },
        &mut NullProgress,
    )
    .expect("dry run");
    let grit_preview: BTreeSet<String> = preview
        .removed
        .iter()
        .map(|p| normalize_clean_path(p))
        .collect();
    assert_eq!(
        grit_preview,
        git_clean_preview(git_root.path(), &[]),
        "preview must match git clean -nd"
    );

    clean_untracked(
        &repo,
        &CleanOptions {
            directories: true,
            dry_run: false,
            ..CleanOptions::default()
        },
        &mut NullProgress,
    )
    .expect("force clean");

    git_clean_force(git_root.path(), &[]);

    assert!(grit_root.path().join("mixed/ignored.log").exists());
    assert!(git_root.path().join("mixed/ignored.log").exists());
    assert_eq!(
        worktree_file_set(grit_root.path()),
        worktree_file_set(git_root.path())
    );
}

#[test]
fn nested_repo_directory_is_untouched() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    init_tracked_repo(root);
    let nested = root.join("nested-repo");
    fs::create_dir_all(&nested).unwrap();
    open_nested_repo(&nested);
    fs::write(nested.join("inside.txt"), b"stay\n").unwrap();
    fs::write(root.join("loose.txt"), b"go\n").unwrap();

    let repo = Repository::discover(Some(root)).expect("open");
    clean_untracked(
        &repo,
        &CleanOptions {
            directories: true,
            dry_run: false,
            ..CleanOptions::default()
        },
        &mut NullProgress,
    )
    .expect("clean");

    assert!(nested.join(".git").exists());
    assert!(nested.join("inside.txt").exists());
    assert!(!root.join("loose.txt").exists());
}

#[test]
fn pathspec_limits_removal() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    init_tracked_repo(root);
    fs::write(root.join("keep.txt"), b"k\n").unwrap();
    fs::create_dir_all(root.join("only")).unwrap();
    fs::write(root.join("only/remove.txt"), b"r\n").unwrap();

    let repo = Repository::discover(Some(root)).expect("open");
    clean_untracked(
        &repo,
        &CleanOptions {
            pathspecs: vec!["only".to_owned()],
            directories: true,
            dry_run: false,
            ..CleanOptions::default()
        },
        &mut NullProgress,
    )
    .expect("clean scoped");

    assert!(!root.join("only/remove.txt").exists());
    assert!(root.join("keep.txt").exists());
}
