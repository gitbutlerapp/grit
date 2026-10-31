//! Pathspec strings (`.`, `./`, `dir/`, globs) are selectors only; path validation runs on staged paths.

use grit_lib::error::Error;
use grit_lib::index::Index;
use grit_lib::porcelain::add::{stage, StageMode, StageOptions};
use grit_lib::progress::NullProgress;
use grit_lib::repo::{InitOptions, Repository};
use std::fs;

fn init_repo() -> (tempfile::TempDir, Repository) {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = Repository::init(dir.path(), &InitOptions::default()).expect("init");
    (dir, repo)
}

fn load_repo_index(repo: &Repository) -> Index {
    Index::load(&repo.index_handle().path()).expect("load index")
}

fn stage_paths(repo: &Repository, pathspecs: &[&str]) -> grit_lib::error::Result<()> {
    let opts = StageOptions {
        pathspecs: pathspecs.iter().map(|s| (*s).to_string()).collect(),
        pathspec_sources: pathspecs.iter().map(|s| (*s).to_string()).collect(),
        mode: StageMode::All,
        ..StageOptions::default()
    };
    stage(
        repo,
        &opts,
        &mut NullProgress,
        &grit_lib::progress::NeverCancel,
    )?;
    Ok(())
}

#[test]
fn add_dot_and_dot_slash_pathspecs_stage_tracked_and_untracked() {
    let (_dir, repo) = init_repo();
    fs::write(repo.work_tree().unwrap().join("tracked.txt"), b"1\n").unwrap();
    fs::write(repo.work_tree().unwrap().join("new.txt"), b"2\n").unwrap();
    grit_test_support::git(repo.work_tree().unwrap(), &["add", "tracked.txt"]);
    grit_test_support::git(repo.work_tree().unwrap(), &["commit", "-m", "init"]);

    for spec in [".", "./"] {
        fs::write(repo.work_tree().unwrap().join("new.txt"), b"updated\n").unwrap();
        stage_paths(&repo, &[spec]).expect("stage {spec}");
        let index = load_repo_index(&repo);
        assert!(
            index.get(b"new.txt", 0).is_some(),
            "pathspec {spec} should stage new.txt"
        );
    }
}

#[test]
fn add_directory_trailing_slash_pathspec_stages_under_prefix() {
    let (_dir, repo) = init_repo();
    let wt = repo.work_tree().unwrap();
    fs::create_dir_all(wt.join("dir")).unwrap();
    fs::write(wt.join("dir/inside.txt"), b"x\n").unwrap();
    stage_paths(&repo, &["dir/"]).expect("add dir/");
    let index = load_repo_index(&repo);
    assert!(index.get(b"dir/inside.txt", 0).is_some());
}

#[test]
fn add_attr_pathspec_stages_matching_modified_and_errors_when_nothing_matches() {
    let (_dir, repo) = init_repo();
    let wt = repo.work_tree().unwrap();
    fs::write(wt.join(".gitattributes"), "*.c myattr\n").unwrap();
    fs::write(wt.join("a.c"), b"v1\n").unwrap();
    fs::write(wt.join("b.txt"), b"note\n").unwrap();
    grit_test_support::git(wt, &["add", "."]);
    grit_test_support::git(wt, &["commit", "-m", "init"]);

    fs::write(wt.join("a.c"), b"v2\n").unwrap();
    fs::write(wt.join("b.txt"), b"changed\n").unwrap();
    stage_paths(&repo, &[":(attr:myattr)"]).expect("stage attr");
    let wt = repo.work_tree().unwrap();
    let staged = grit_test_support::git(wt, &["diff", "--cached", "--name-only"]);
    assert_eq!(staged.trim(), "a.c", "only *.c paths match :(attr:myattr)");
    let unstaged = grit_test_support::git(wt, &["diff", "--name-only"]);
    assert!(
        unstaged.lines().any(|l| l == "b.txt"),
        "b.txt should remain unstaged: {unstaged}"
    );

    fs::write(wt.join("b.txt"), b"again\n").unwrap();
    let err = stage_paths(&repo, &[":(attr:no-such)"]).expect_err("no match");
    assert!(
        matches!(err, Error::Message(ref m) if m.contains("did not match any files")),
        "{err:?}"
    );
}

#[test]
fn add_attr_pathspec_with_trailing_directory_pattern_stages_under_prefix() {
    let (_dir, repo) = init_repo();
    let wt = repo.work_tree().unwrap();
    fs::create_dir_all(wt.join("a")).unwrap();
    fs::write(wt.join(".gitattributes"), "*.c myattr\n").unwrap();
    fs::write(wt.join("a/g.c"), b"v1\n").unwrap();
    fs::write(wt.join("b.txt"), b"note\n").unwrap();
    grit_test_support::git(wt, &["add", "."]);
    grit_test_support::git(wt, &["commit", "-m", "init"]);

    fs::write(wt.join("a/g.c"), b"v2\n").unwrap();
    fs::write(wt.join("b.txt"), b"changed\n").unwrap();
    stage_paths(&repo, &[":(attr:myattr)a/"]).expect("stage attr under a/");
    let staged = grit_test_support::git(wt, &["diff", "--cached", "--name-only"]);
    assert_eq!(
        staged.trim(),
        "a/g.c",
        "attr + directory prefix must stage matching paths"
    );
}

#[test]
fn add_glob_pathspec_stages_matching_untracked() {
    let (_dir, repo) = init_repo();
    let wt = repo.work_tree().unwrap();
    fs::write(wt.join("a.rs"), b"fn main() {}\n").unwrap();
    fs::write(wt.join("b.txt"), b"note\n").unwrap();
    stage_paths(&repo, &["*.rs"]).expect("add glob");
    let index = load_repo_index(&repo);
    assert!(index.get(b"a.rs", 0).is_some());
    assert!(index.get(b"b.txt", 0).is_none());
}

#[test]
fn add_from_subdirectory_with_dot_pathspec() {
    let (_dir, repo) = init_repo();
    let wt = repo.work_tree().unwrap();
    fs::create_dir_all(wt.join("sub")).unwrap();
    fs::write(wt.join("sub/local.txt"), b"local\n").unwrap();
    let prev = std::env::current_dir().expect("cwd");
    std::env::set_current_dir(wt.join("sub")).expect("chdir sub");
    let result = stage_paths(&repo, &["."]);
    std::env::set_current_dir(prev).expect("restore cwd");
    result.expect("add . from subdir");
    let index = load_repo_index(&repo);
    assert!(index.get(b"sub/local.txt", 0).is_some());
}

#[test]
fn pathless_add_rejects_hostile_expanded_path() {
    let (_dir, repo) = init_repo();
    let wt = repo.work_tree().unwrap();
    fs::write(wt.join("git~1"), b"evil\n").unwrap();
    fs::write(wt.join("ok.txt"), b"ok\n").unwrap();
    let err = stage_paths(&repo, &[]).expect_err("pathless add with git~1");
    assert!(
        matches!(err, Error::InvalidPathComponent { .. }),
        "expected InvalidPathComponent, got {err:?}"
    );
}

#[test]
fn glob_add_rejects_hostile_match_before_index_insert() {
    let (_dir, repo) = init_repo();
    let wt = repo.work_tree().unwrap();
    fs::write(wt.join("git~1"), b"evil\n").unwrap();
    fs::write(wt.join("good.txt"), b"ok\n").unwrap();
    let err = stage_paths(&repo, &["*"]).expect_err("glob should still validate git~1");
    assert!(
        matches!(err, Error::InvalidPathComponent { .. }),
        "expected InvalidPathComponent, got {err:?}"
    );
}
