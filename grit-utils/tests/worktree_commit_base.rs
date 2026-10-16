//! Worktree commit prepare must not advance HEAD before the timed commit.

use std::path::Path;

use grit_utils::fixture_registry::{self, FixtureKind};
use grit_utils::worktree_fixture::{
    ensure_worktree_bench_repo, prepare_commit, read_fixture_base_oid, set_worktree_bench_cwd,
};

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
