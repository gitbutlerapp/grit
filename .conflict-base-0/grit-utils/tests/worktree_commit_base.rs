//! Worktree commit prepare hooks and fixture-base reset behavior.

use std::path::Path;
use std::process::Command;

use grit_utils::fixture_registry::{self, FixtureKind};
use grit_utils::worktree_fixture::{
    ensure_worktree_bench_repo, prepare_commit, read_fixture_base_oid, reset_to_fixture_base,
    set_worktree_bench_cwd,
};
use tempfile::TempDir;

#[test]
fn commit_prepare_resets_to_fixture_base_after_git_commits() {
    let cache = TempDir::new().expect("tempdir");
    let scratch = TempDir::new().expect("tempdir");
    std::env::set_var("GRIT_BENCH_CACHE", cache.path());
    std::env::set_var("GRIT_BENCH_SCRATCH", scratch.path());

    let git = grit_utils::binary::resolve_binary("git", None).expect("git");
    fixture_registry::ensure_cached(&git, FixtureKind::SynthS, false).expect("cache");

    let (repo, _) = ensure_worktree_bench_repo(&git, FixtureKind::SynthS, true).expect("repo");
    set_worktree_bench_cwd(&repo).expect("cwd marker");
    let base = read_fixture_base_oid().expect("base oid");

    prepare_commit(&git).expect("first prepare");
    Command::new(&git)
        .args(["add", "-A"])
        .current_dir(&repo)
        .status()
        .expect("git add");
    Command::new(&git)
        .args(["commit", "-q", "-m", "advance"])
        .current_dir(&repo)
        .status()
        .expect("git commit");
    let advanced = fixture_registry::git_head_oid(&git, &repo).expect("head");
    assert_ne!(advanced, base, "test setup must advance HEAD");

    prepare_commit(&git).expect("second prepare after git commits");
    let after = fixture_registry::git_head_oid(&git, &repo).expect("head after prepare");
    assert_eq!(
        after, base,
        "prepare must restore fixture base for grit iterations"
    );

    reset_to_fixture_base(&git, &repo).expect("cleanup");
}

#[test]
fn worktree_commit_prepare_leaves_head_at_fixture_base() {
    let git = grit_utils::binary::resolve_binary("git", None).expect("git");
    let (repo, _record) =
        ensure_worktree_bench_repo(&git, FixtureKind::SynthS, false).expect("worktree repo");
    set_worktree_bench_cwd(&repo).expect("cwd");
    let base = read_fixture_base_oid().expect("base oid");
    prepare_commit(&git).expect("prepare commit");
    let head = fixture_registry::git_head_oid(&git, &repo).expect("head");
    assert_eq!(
        head, base,
        "prepare must not create a commit before hyperfine runs"
    );
    let _ = Path::new(".");
}
