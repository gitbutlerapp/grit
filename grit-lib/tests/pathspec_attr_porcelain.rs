//! `:(attr:...)` pathspecs on restore, stash push, and `rm --cached` (issue #2417).

use grit_lib::worktree::WorktreeError;
use std::fs;
use std::process::Command;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::error::Error;
use grit_lib::porcelain::paths::RemoveOptions;
use grit_lib::porcelain::restore::RestoreOptions;
use grit_lib::progress::{CancelToken, NullProgress};
use grit_lib::repo::Repository;
use grit_lib::stash::{StashMode, StashPushOptions};
use tempfile::TempDir;

fn git(root: &std::path::Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(root)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn open_repo(root: &std::path::Path) -> Repository {
    Repository::discover(Some(root), &RepositoryOptions::empty()).expect("discover")
}

fn vendored_fixture() -> TempDir {
    let dir = TempDir::new().expect("tempdir");
    let root = dir.path();
    git(root, &["init", "-q", "-b", "main", "."]);
    git(root, &["config", "user.email", "t@example.com"]);
    git(root, &["config", "user.name", "Test"]);
    fs::write(root.join(".gitattributes"), "*.rs vendored\n").unwrap();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/a.rs"), "1\n").unwrap();
    fs::write(root.join("src/b.txt"), "1\n").unwrap();
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "init"]);
    fs::write(root.join("src/a.rs"), "1\n2\n").unwrap();
    fs::write(root.join("src/b.txt"), "1\n2\n").unwrap();
    dir
}

fn restore_opts(pathspec: &str, staged: bool, worktree: bool) -> RestoreOptions {
    RestoreOptions::new(vec![pathspec.into()])
        .with_pathspec_sources(vec![pathspec.into()])
        .with_staged(staged)
        .with_worktree(worktree)
}

#[test]
fn restore_attr_pathspec_worktree_and_staged() {
    let dir = vendored_fixture();
    let root = dir.path();
    let repo = open_repo(root);
    repo.checkout()
        .restore(
            &restore_opts(":(attr:vendored)src", false, true),
            &mut grit_lib::progress::NullProgress,
            &grit_lib::progress::CancelToken::never(),
        )
        .expect("restore worktree");
    assert_eq!(fs::read_to_string(root.join("src/a.rs")).unwrap(), "1\n");
    assert_eq!(
        fs::read_to_string(root.join("src/b.txt")).unwrap(),
        "1\n2\n"
    );

    fs::write(root.join("src/a.rs"), "1\n2\n").unwrap();
    git(root, &["add", "src/a.rs"]);
    repo.checkout()
        .restore(
            &restore_opts(":(attr:vendored)src", true, false),
            &mut grit_lib::progress::NullProgress,
            &grit_lib::progress::CancelToken::never(),
        )
        .expect("restore staged");
    assert!(git(root, &["diff", "--cached", "--name-only"])
        .trim()
        .is_empty());
}

#[test]
fn restore_attr_pathspec_no_match() {
    let dir = vendored_fixture();
    let repo = open_repo(dir.path());
    let err = repo
        .checkout()
        .restore(
            &restore_opts(":(attr:no-such)src", false, true),
            &mut grit_lib::progress::NullProgress,
            &grit_lib::progress::CancelToken::never(),
        )
        .expect_err("no match");
    assert!(matches!(
        err,
        Error::Worktree(WorktreeError::PathspecNoMatch { .. })
    ));
}

#[test]
fn stash_push_attr_pathspec() {
    let dir = vendored_fixture();
    let root = dir.path();
    let repo = open_repo(root);
    repo.stash()
        .push(
            &StashPushOptions {
                message: Some("attr".into()),
                include_untracked: false,
                identity: "Test <t@example.com> 1000000000 +0000".parse().unwrap(),
                selection: None,
                pathspecs: vec![":(attr:vendored)src".into()],
                dry_run: false,
                mode: StashMode::All,
            },
            &mut grit_lib::progress::NullProgress,
            &grit_lib::progress::CancelToken::never(),
        )
        .expect("stash");
    assert_eq!(fs::read_to_string(root.join("src/a.rs")).unwrap(), "1\n");
    assert_eq!(
        git(root, &["stash", "show", "--name-only"]).trim(),
        "src/a.rs"
    );
}

#[test]
fn objectmode_attr_pathspec_restore_stash_rm() {
    let dir = TempDir::new().expect("tempdir");
    let root = dir.path();
    git(root, &["init", "-q", "-b", "main", "."]);
    git(root, &["config", "user.email", "t@example.com"]);
    git(root, &["config", "user.name", "Test"]);
    fs::write(root.join("run.sh"), "#!/bin/sh\n").unwrap();
    fs::write(root.join("plain.txt"), "x\n").unwrap();
    git(root, &["add", "--chmod=+x", "run.sh"]);
    git(root, &["add", "plain.txt"]);
    git(root, &["commit", "-qm", "init"]);
    fs::write(root.join("run.sh"), "#!/bin/sh\necho\n").unwrap();
    fs::write(root.join("plain.txt"), "x\ny\n").unwrap();

    let repo = open_repo(root);
    let spec = ":(attr:builtin_objectmode=100755)";

    repo.checkout()
        .restore(
            &restore_opts(spec, false, true),
            &mut grit_lib::progress::NullProgress,
            &grit_lib::progress::CancelToken::never(),
        )
        .expect("restore executable only");
    assert_eq!(
        fs::read_to_string(root.join("run.sh")).unwrap(),
        "#!/bin/sh\n"
    );
    assert_eq!(
        fs::read_to_string(root.join("plain.txt")).unwrap(),
        "x\ny\n"
    );

    fs::write(root.join("run.sh"), "#!/bin/sh\necho\n").unwrap();
    repo.stash()
        .push(
            &StashPushOptions {
                message: Some("mode".into()),
                include_untracked: false,
                identity: "Test <t@example.com> 1000000000 +0000".parse().unwrap(),
                selection: None,
                pathspecs: vec![spec.into()],
                dry_run: false,
                mode: StashMode::All,
            },
            &mut grit_lib::progress::NullProgress,
            &grit_lib::progress::CancelToken::never(),
        )
        .expect("stash executable");
    assert_eq!(
        git(root, &["stash", "show", "--name-only"]).trim(),
        "run.sh"
    );

    git(root, &["stash", "drop"]);
    fs::write(root.join("run.sh"), "#!/bin/sh\necho\n").unwrap();
    git(root, &["add", "run.sh"]);
    repo.checkout()
        .remove_paths(
            &RemoveOptions {
                pathspecs: vec![spec.into()],
                pathspec_sources: vec![spec.into()],
                cached: true,
                ..Default::default()
            },
            &grit_lib::progress::CancelToken::never(),
        )
        .expect("rm --cached executable");
    let ls = git(root, &["ls-files"]);
    assert!(ls.contains("plain.txt"));
    assert!(!ls.contains("run.sh"));
}

#[test]
fn rm_cached_attr_pathspec() {
    let dir = vendored_fixture();
    let root = dir.path();
    let repo = open_repo(root);
    repo.checkout()
        .remove_paths(
            &RemoveOptions {
                pathspecs: vec![":(attr:vendored)src".into()],
                pathspec_sources: vec![":(attr:vendored)src".into()],
                cached: true,
                ..Default::default()
            },
            &grit_lib::progress::CancelToken::never(),
        )
        .expect("rm --cached");
    let ls = git(root, &["ls-files"]);
    assert!(ls.contains("src/b.txt"));
    assert!(!ls.contains("src/a.rs"));
    assert!(root.join("src/a.rs").is_file());
}
