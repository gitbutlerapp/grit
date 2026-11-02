//! `diff.ignoreSubmodules` / `submodule.*.ignore` parity with system git (issue #2524).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::diff::{
    diff_index_to_worktree_with_options, diff_tree_to_worktree, DiffIndexToWorktreeOptions,
    RenameDetection, UncommittedDiff,
};
use grit_lib::porcelain::status::{status, StatusOptions};
use grit_lib::progress::{NeverCancel, NullProgress};
use grit_lib::repo::Repository;
use grit_lib::stash::StashPushOptions;
use grit_test_support::git;
use grit_test_support::git_cmd;
use tempfile::tempdir;

fn git_status_short(dir: &Path, ignore: &str) -> String {
    let ignore_cfg = format!("diff.ignoreSubmodules={ignore}");
    git_cmd(&["-c", ignore_cfg.as_str(), "status", "--short"])
        .in_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .suc()
        .stdout
}

fn setup_submodule_parent(dirty_tracked: bool) -> tempfile::TempDir {
    let root = tempdir().expect("tempdir");
    let lib = root.path().join("lib");
    std::fs::create_dir(&lib).expect("lib dir");
    git(lib.as_path(), &["init", "-q"]);
    git(lib.as_path(), &["config", "user.email", "a@b"]);
    git(lib.as_path(), &["config", "user.name", "A"]);
    std::fs::write(lib.join("l.txt"), "l1\n").expect("lib file");
    git(lib.as_path(), &["add", "."]);
    git(lib.as_path(), &["commit", "-qm", "l1"]);

    let main = root.path().join("main");
    std::fs::create_dir(&main).expect("main dir");
    git(main.as_path(), &["init", "-q"]);
    git(main.as_path(), &["config", "user.email", "a@b"]);
    git(main.as_path(), &["config", "user.name", "A"]);
    let lib_url = format!(
        "file://{}",
        lib.canonicalize()
            .expect("lib path")
            .display()
            .to_string()
            .replace('\\', "/")
    );
    git_cmd(&[
        "-c",
        "protocol.file.allow=always",
        "submodule",
        "add",
        "-q",
        lib_url.as_str(),
        "vendor/lib",
    ])
    .in_dir(main.as_path())
    .suc();
    git(main.as_path(), &["commit", "-qm", "sub"]);
    // Override a global `diff.ignoreSubmodules` (common in dogfooding configs) for baseline checks.
    git(main.as_path(), &["config", "diff.ignoreSubmodules", "none"]);
    if dirty_tracked {
        let sub_l = main.join("vendor/lib/l.txt");
        let mut content = std::fs::read_to_string(&sub_l).expect("submodule tracked file");
        content.push_str("dirty\n");
        std::fs::write(&sub_l, content).expect("dirty submodule file");
    }
    root
}

fn grit_repo(main: &Path) -> Repository {
    Repository::open(
        &main.join(".git"),
        &RepositoryOptions::empty().with_work_tree(main),
    )
    .expect("open grit repo")
}

#[test]
fn dirty_submodule_only_reported_without_ignore() {
    let root = setup_submodule_parent(true);
    let main = root.path().join("main");
    assert!(
        std::fs::read_to_string(main.join("vendor/lib/l.txt"))
            .expect("submodule file")
            .contains("dirty"),
        "submodule worktree should be dirty"
    );
    let git_short = git_status_short(main.as_path(), "none");
    assert!(
        git_short.contains("vendor/lib"),
        "git should report dirty submodule: {git_short:?}"
    );

    let repo = grit_repo(&main);
    let model = status(
        &repo,
        &StatusOptions::default(),
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("status");
    assert!(
        model.unstaged.iter().any(|e| e.path() == "vendor/lib"),
        "expected unstaged gitlink before ignore config"
    );

    git(main.as_path(), &["config", "diff.ignoreSubmodules", "all"]);
    let git_short = git_status_short(main.as_path(), "all");
    assert!(
        git_short.trim().is_empty(),
        "git should hide dirty-only submodule with ignore=all: {git_short:?}"
    );

    let repo = grit_repo(&main);
    let model = status(
        &repo,
        &StatusOptions::default(),
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("status");
    assert!(
        model.unstaged.is_empty() && model.staged.is_empty(),
        "grit status should match git with diff.ignoreSubmodules=all"
    );

    let index = repo.index_handle().load().expect("index");
    let diff = diff_tree_to_worktree(repo.odb(), model.head_tree.as_ref(), &main, &index)
        .expect("tree diff");
    assert!(
        diff.is_empty(),
        "grit diff should omit gitlink with ignore=all, got {diff:?}"
    );
}

#[test]
fn stash_skips_dirty_submodule_only_worktree() {
    let root = setup_submodule_parent(true);
    let main = root.path().join("main");
    let git_out = git(main.as_path(), &["stash"]);
    assert!(
        git_out.contains("No local changes to save"),
        "git stash: {git_out:?}"
    );

    let repo = grit_repo(&main);
    let opts = StashPushOptions {
        message: None,
        include_untracked: false,
        identity: "Test <t@example.com> 1000000000 +0000"
            .parse()
            .expect("signature"),
        selection: None,
        pathspecs: Vec::new(),
        dry_run: false,
        mode: grit_lib::porcelain::stash::StashMode::default(),
    };
    let pushed = repo
        .stash()
        .push(&opts, &mut NullProgress, &NeverCancel)
        .expect("stash push");
    assert!(
        pushed.is_none(),
        "grit stash should not create an entry for dirty-only submodule"
    );
}

#[test]
fn moved_submodule_head_honors_ignore_dirty_not_all() {
    let root = setup_submodule_parent(false);
    let main = root.path().join("main");
    let sub = main.join("vendor/lib");
    std::fs::write(sub.join("x.txt"), "x\n").expect("new file in sub");
    git(sub.as_path(), &["add", "x.txt"]);
    git(sub.as_path(), &["commit", "-m", "inner"]);

    git(
        main.as_path(),
        &["config", "diff.ignoreSubmodules", "dirty"],
    );
    let git_dirty = git_status_short(main.as_path(), "dirty");
    assert!(
        git_dirty.contains('M') && git_dirty.contains("vendor/lib"),
        "git should report moved submodule HEAD with ignore=dirty: {git_dirty:?}"
    );
    let repo = grit_repo(&main);
    let model = status(
        &repo,
        &StatusOptions::default(),
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("status");
    assert!(
        model.unstaged.iter().any(|e| e.path() == "vendor/lib"),
        "grit should report moved submodule HEAD with ignore=dirty"
    );

    git(main.as_path(), &["config", "diff.ignoreSubmodules", "all"]);
    let git_all = git_status_short(main.as_path(), "all");
    assert!(
        git_all.trim().is_empty(),
        "git omits moved submodule HEAD from short status with ignore=all: {git_all:?}"
    );
    let repo = grit_repo(&main);
    let model = status(
        &repo,
        &StatusOptions::default(),
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("status");
    assert!(
        model.unstaged.is_empty(),
        "grit should match git: no unstaged gitlink with ignore=all when only HEAD moved"
    );
}

#[test]
fn short_status_uses_lowercase_m_for_content_dirty_gitlink() {
    let root = setup_submodule_parent(true);
    let main = root.path().join("main");
    let git_short = git_status_short(main.as_path(), "none");
    assert!(
        git_short.contains(" m vendor/lib") || git_short.contains("\tm vendor/lib"),
        "git short status for content-dirty gitlink: {git_short:?}"
    );

    let repo = grit_repo(&main);
    let model = status(
        &repo,
        &StatusOptions::default(),
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("status");
    let lines = grit_lib::porcelain::status::short_status_lines(&model);
    let line = lines
        .iter()
        .find(|l| l.render_with(|p| p.to_string()).contains("vendor/lib"))
        .expect("vendor/lib line");
    let rendered = line.render_with(|p| p.to_string());
    assert!(
        rendered.contains("m vendor/lib") && !rendered.contains('M'),
        "expected lowercase m for content-dirty submodule, got {rendered:?}"
    );
}

#[test]
fn ignore_untracked_only_inside_submodule() {
    let root = setup_submodule_parent(false);
    let main = root.path().join("main");
    std::fs::write(main.join("vendor/lib/new.txt"), "u\n").expect("untracked in sub");
    git(
        main.as_path(),
        &["config", "diff.ignoreSubmodules", "untracked"],
    );

    let git_short = git_status_short(main.as_path(), "untracked");
    assert!(
        git_short.trim().is_empty(),
        "git should ignore untracked-only inside submodule: {git_short:?}"
    );

    let repo = grit_repo(&main);
    let mut index = repo.index_handle().load().expect("index");
    let (diff, _) = diff_index_to_worktree_with_options(
        repo.odb(),
        &mut index,
        &main,
        DiffIndexToWorktreeOptions {
            repository_git_dir: Some(repo.git_dir().to_path_buf()),
            config: repo.config().ok(),
            ..DiffIndexToWorktreeOptions::default()
        },
    )
    .expect("index diff");
    assert!(diff.is_empty(), "grit diff should honor ignore=untracked");
}

fn setup_submodule_logical_name_ignore_all() -> tempfile::TempDir {
    let root = tempdir().expect("tempdir");
    let lib = root.path().join("lib");
    std::fs::create_dir(&lib).expect("lib dir");
    git(lib.as_path(), &["init", "-q"]);
    git(lib.as_path(), &["config", "user.email", "a@b"]);
    git(lib.as_path(), &["config", "user.name", "A"]);
    std::fs::write(lib.join("l.txt"), "l1\n").expect("lib file");
    git(lib.as_path(), &["add", "."]);
    git(lib.as_path(), &["commit", "-qm", "l1"]);

    let main = root.path().join("main");
    std::fs::create_dir(&main).expect("main dir");
    git(main.as_path(), &["init", "-q"]);
    git(main.as_path(), &["config", "user.email", "a@b"]);
    git(main.as_path(), &["config", "user.name", "A"]);
    let lib_url = format!(
        "file://{}",
        lib.canonicalize()
            .expect("lib path")
            .display()
            .to_string()
            .replace('\\', "/")
    );
    git_cmd(&[
        "-c",
        "protocol.file.allow=always",
        "submodule",
        "add",
        "--name",
        "logical",
        "-q",
        lib_url.as_str(),
        "vendor/lib",
    ])
    .in_dir(main.as_path())
    .suc();
    let gitmodules = std::fs::read_to_string(main.join(".gitmodules")).expect("gitmodules");
    let gitmodules = format!("{gitmodules}\tignore = all\n");
    std::fs::write(main.join(".gitmodules"), gitmodules).expect("gitmodules ignore");
    git(main.as_path(), &["add", ".gitmodules", "vendor/lib"]);
    git(main.as_path(), &["commit", "-qm", "sub"]);
    git(main.as_path(), &["config", "diff.ignoreSubmodules", "none"]);
    let sub_l = main.join("vendor/lib/l.txt");
    let mut content = std::fs::read_to_string(&sub_l).expect("submodule tracked file");
    content.push_str("dirty\n");
    std::fs::write(&sub_l, content).expect("dirty submodule file");
    root
}

#[test]
fn gitmodules_logical_name_ignore_all_hides_dirty_submodule() {
    let root = setup_submodule_logical_name_ignore_all();
    let main = root.path().join("main");

    let git_short = git_status_short(main.as_path(), "none");
    assert!(
        git_short.trim().is_empty(),
        "git should honor .gitmodules ignore=all via logical name: {git_short:?}"
    );

    let repo = grit_repo(&main);
    let model = status(
        &repo,
        &StatusOptions::default(),
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("status");
    assert!(
        model.unstaged.is_empty(),
        "grit status should resolve logical submodule name from .gitmodules"
    );
}

#[test]
fn staged_gitlink_visible_in_status_hidden_in_diff_with_ignore_all() {
    let root = setup_submodule_parent(false);
    let main = root.path().join("main");
    let sub = main.join("vendor/lib");
    std::fs::write(sub.join("x.txt"), "x\n").expect("sub file");
    git(sub.as_path(), &["add", "x.txt"]);
    git(sub.as_path(), &["commit", "-m", "inner"]);
    git(main.as_path(), &["add", "vendor/lib"]);
    git(main.as_path(), &["config", "diff.ignoreSubmodules", "all"]);

    let git_status = git_status_short(main.as_path(), "all");
    assert!(
        git_status.contains("vendor/lib"),
        "git status still lists staged gitlink with ignore=all: {git_status:?}"
    );
    let git_cached = git_cmd(&[
        "-c",
        "diff.ignoreSubmodules=all",
        "diff",
        "--cached",
        "--stat",
    ])
    .in_dir(main.as_path())
    .env("GIT_CONFIG_GLOBAL", "/dev/null")
    .env("GIT_CONFIG_SYSTEM", "/dev/null")
    .suc()
    .stdout;
    assert!(
        git_cached.trim().is_empty(),
        "git cached diff empty with ignore=all: {git_cached:?}"
    );

    let repo = grit_repo(&main);
    let model = status(
        &repo,
        &StatusOptions::default(),
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("status");
    assert!(
        model.staged.iter().any(|e| e.path() == "vendor/lib"),
        "grit status staged gitlink with ignore=all"
    );

    let diff = repo.diff().uncommitted().expect("diff");
    let staged = diff
        .staged_entries(RenameDetection::Off)
        .expect("staged entries");
    assert!(
        staged.is_empty(),
        "grit staged diff should omit gitlink when ignore=all, got {staged:?}"
    );
}
