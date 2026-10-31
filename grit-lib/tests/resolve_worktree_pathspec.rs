//! `Repository::resolve_worktree_pathspec` and `ResolvedPathspecs::resolve` (issue #1682).
//!
//! Resolving argv pathspecs must cost no config reload or `.git` filesystem probe per path:
//! `grit add <98 paths>` on macOS repeated both for every argument.

use grit_lib::repo::{InitOptions, Repository};
use grit_lib::worktree::pathspec::{PathspecResolveError, ResolvedPathspecs};
use std::fs;

fn init_repo() -> (tempfile::TempDir, Repository) {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = Repository::init(dir.path(), &InitOptions::default()).expect("init");
    (dir, repo)
}

#[test]
fn resolves_relative_to_prefix_and_normalizes() {
    let (_dir, repo) = init_repo();
    assert_eq!(
        repo.resolve_worktree_pathspec("a.txt", None).expect("root"),
        "a.txt"
    );
    assert_eq!(
        repo.resolve_worktree_pathspec("a.txt", Some("sub"))
            .expect("prefix"),
        "sub/a.txt"
    );
    assert_eq!(
        repo.resolve_worktree_pathspec("../b/./c.txt", Some("sub/deep"))
            .expect("dotdot"),
        "sub/b/c.txt"
    );
}

#[test]
fn magic_pathspecs_pass_through() {
    let (_dir, repo) = init_repo();
    assert_eq!(
        repo.resolve_worktree_pathspec(":(exclude)vendor", None)
            .expect("long magic"),
        ":(exclude)vendor"
    );
    assert_eq!(
        repo.resolve_worktree_pathspec(":(exclude)vendor", Some("sub"))
            .expect("long magic under prefix"),
        ":(exclude,prefix:sub/)vendor"
    );
}

/// Regression for #2355: `:(top)` must not gain `prefix:<cwd>/` when cwd is a subdirectory.
#[test]
fn top_magic_pathspecs_stay_repo_root_relative_from_prefix() {
    let (_dir, repo) = init_repo();
    let prefix = "a/b";
    let cases = [":(top)a/g.c", ":(top,glob)a/**/*.c", ":(exclude,top)a/b"];
    for spec in cases {
        assert_eq!(
            repo.resolve_worktree_pathspec(spec, Some(prefix))
                .expect("resolve"),
            spec,
            "spec must not be cwd-prefixed"
        );
    }
    let (dir, repo) = init_repo();
    let sub = dir.path().join("a/b");
    fs::create_dir_all(&sub).expect("sub");
    fs::write(dir.path().join("a/g.c"), b"1\n").expect("g.c");
    fs::write(sub.join("h.c"), b"1\n").expect("h.c");
    let raw = vec![":(top)a/g.c".to_string()];
    let resolved = ResolvedPathspecs::resolve(&repo, &sub, &raw, None).expect("resolve batch");
    assert_eq!(resolved.specs, raw);
}

#[test]
fn absolute_path_outside_work_tree_is_a_typed_error() {
    let (_dir, repo) = init_repo();
    let other = tempfile::tempdir().expect("other tempdir");
    let outside = other.path().join("elsewhere").display().to_string();
    let err = repo
        .resolve_worktree_pathspec(&outside, None)
        .expect_err("outside");
    assert!(
        matches!(&err, PathspecResolveError::OutsideRepository { elt, .. } if *elt == outside),
        "{err:?}"
    );
    let inside = repo.work_tree().expect("work tree").join("in.txt");
    assert_eq!(
        repo.resolve_worktree_pathspec(&inside.display().to_string(), None)
            .expect("absolute inside"),
        "in.txt"
    );
}

#[test]
fn bare_repository_needs_work_tree() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo =
        Repository::init(dir.path(), &InitOptions::default().with_bare(true)).expect("init bare");
    assert_eq!(
        repo.resolve_worktree_pathspec("a", None),
        Err(PathspecResolveError::NeedsWorkTree)
    );
}

#[test]
fn resolved_pathspecs_match_per_path_resolution() {
    let (dir, repo) = init_repo();
    let raw: Vec<String> = (0..20).map(|i| format!("d{i}/f.txt")).collect();
    let resolved =
        ResolvedPathspecs::resolve(&repo, &dir.path().join("sub"), &raw, None).expect("resolve");
    let one_by_one: Vec<String> = raw
        .iter()
        .map(|p| repo.resolve_worktree_pathspec(p, Some("sub")).expect("one"))
        .collect();
    assert_eq!(resolved.specs, one_by_one);
    assert_eq!(resolved.sources, raw);
}

/// On main, every argv pathspec in a `core.precomposeunicode=true` repository (the macOS default)
/// reloaded config and created + unlinked an NFD probe file in `.git`, bumping its mtime.
#[cfg(unix)]
#[test]
fn many_pathspecs_do_not_reprobe_git_dir_per_path() {
    let (dir, _repo) = init_repo();
    grit_test_support::git(dir.path(), &["config", "core.precomposeunicode", "true"]);
    let repo = Repository::discover(Some(dir.path()), &Default::default()).expect("reopen");
    repo.resolve_worktree_pathspec("warm", None).expect("warm");

    let git_dir = repo.git_dir().to_path_buf();
    let old = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000_000);
    fs::File::open(&git_dir)
        .expect("open .git")
        .set_modified(old)
        .expect("set .git mtime");

    let raw: Vec<String> = (0..100).map(|i| format!("dir{i}/file.txt")).collect();
    let resolved = ResolvedPathspecs::resolve(&repo, dir.path(), &raw, None).expect("resolve");
    assert_eq!(resolved.specs, raw);
    for p in &raw {
        repo.resolve_worktree_pathspec(p, None)
            .expect("resolve one");
    }

    let after = fs::metadata(&git_dir)
        .expect("stat .git")
        .modified()
        .expect("mtime");
    assert_eq!(
        after, old,
        "resolving pathspecs must not create files in .git per path"
    );
}
