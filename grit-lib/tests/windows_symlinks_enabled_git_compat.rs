//! Issue #2402: with `core.symlinks=true`, checkout paths must create real symlinks on Windows.

#![cfg(windows)]

use std::fs;
use std::path::{Path, PathBuf};

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::porcelain::restore::{restore_paths, RestoreOptions, RestoreSource};
use grit_lib::porcelain::switch::{switch_repository, SwitchMode, SwitchOptions};
use grit_lib::remote::clone::{clone, CloneOptions};
use grit_lib::repo::Repository;
use grit_test_support::git;

const LINK_TARGET: &str = "target.txt";

fn init_repo_with_symlink(dir: &Path) -> PathBuf {
    git(dir, &["init", "-q", "-b", "main", "."]);
    git(dir, &["config", "user.name", "X"]);
    git(dir, &["config", "user.email", "x@x"]);
    git(dir, &["config", "core.symlinks", "true"]);
    fs::write(dir.join("target.txt"), "hello").expect("write target");
    std::os::windows::fs::symlink_file(LINK_TARGET, dir.join("link.txt")).expect("mklink");
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-qm", "c1"]);
    let mode = git(dir, &["ls-tree", "HEAD", "link.txt"]);
    assert!(
        mode.contains("120000"),
        "expected symlink blob in tree, got {mode}"
    );
    dir.to_path_buf()
}

fn open_repo(root: &Path) -> Repository {
    Repository::open(
        &root.join(".git"),
        &RepositoryOptions::empty().with_work_tree(root),
    )
    .expect("open grit repo")
}

fn assert_link_is_symlink(root: &Path) {
    let link = root.join("link.txt");
    let meta = fs::symlink_metadata(&link).expect("link meta");
    assert!(
        meta.file_type().is_symlink(),
        "link.txt must be a symlink, not a plain file"
    );
    assert_eq!(
        fs::read_link(&link).expect("read_link"),
        Path::new(LINK_TARGET)
    );
}

fn assert_git_clean(root: &Path) {
    let status = git(root, &["status", "-s"]);
    assert_eq!(
        status.trim(),
        "",
        "git status must be clean, got {status:?}"
    );
}

#[test]
fn restore_switch_clone_honor_core_symlinks_true() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let src = tmp.path().join("r");
    fs::create_dir_all(&src).expect("mkdir src");
    init_repo_with_symlink(&src);

    let grit = tmp.path().join("grit-restore");
    fs::create_dir_all(&grit).expect("mkdir grit");
    git(
        tmp.path(),
        &[
            "-c",
            "core.symlinks=true",
            "clone",
            "-q",
            src.to_str().expect("utf8"),
            "grit-restore",
        ],
    );
    git(&grit, &["config", "core.symlinks", "true"]);
    let repo = open_repo(&grit);

    fs::remove_file(grit.join("link.txt")).expect("remove link");
    restore_paths(
        &repo,
        &RestoreOptions {
            pathspecs: vec!["link.txt".to_owned()],
            pathspec_sources: vec!["link.txt".to_owned()],
            source: RestoreSource::Index,
            staged: false,
            worktree: false,
            stage: None,
            remerge: None,
        },
        &mut grit_lib::progress::NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("restore");
    assert_link_is_symlink(&grit);
    assert_git_clean(&grit);

    git(&grit, &["checkout", "-q", "-b", "b2"]);
    git(&grit, &["rm", "-q", "link.txt"]);
    git(&grit, &["commit", "-qm", "rm"]);
    switch_repository(
        &repo,
        SwitchOptions {
            mode: SwitchMode::Existing {
                name: "main".to_owned(),
                start_point: None,
            },
            ..SwitchOptions::default()
        },
        &mut grit_lib::progress::NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("switch main");
    assert_link_is_symlink(&grit);
    assert_git_clean(&grit);

    let clone_dest = tmp.path().join("rc");
    let mut opts = CloneOptions::new(format!("file://{}", src.display()), clone_dest.clone());
    opts.config_overrides
        .push(("core.symlinks".to_owned(), "true".to_owned()));
    clone(
        &opts,
        &mut grit_lib::progress::NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("grit clone");
    assert_link_is_symlink(&clone_dest);
    assert_git_clean(&clone_dest);
}

#[test]
fn core_symlinks_false_still_writes_plain_file() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let src = tmp.path().join("r");
    fs::create_dir_all(&src).expect("mkdir src");
    init_repo_with_symlink(&src);

    let dest = tmp.path().join("plain");
    git(
        tmp.path(),
        &[
            "-c",
            "core.symlinks=false",
            "clone",
            "-q",
            src.to_str().expect("utf8"),
            "plain",
        ],
    );
    git(&dest, &["config", "core.symlinks", "false"]);
    let link = dest.join("link.txt");
    let meta = fs::symlink_metadata(&link).expect("meta");
    assert!(meta.is_file() && !meta.file_type().is_symlink());
    assert_eq!(fs::read_to_string(&link).expect("read"), LINK_TARGET);
}
