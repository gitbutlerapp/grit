//! Fresh-checkout prepare must leave an empty worktree before timed materialization.

use std::process::Command;

use grit_utils::binary::resolve_binary;
use grit_utils::parallel_fixture::{
    assert_clone_no_checkout_worktree, ensure_bare_remote, file_url, parallel_clone_dir,
    worktree_non_git_path_count, ParallelProfile,
};
use grit_utils::parallel_suite::prepare_parallel_checkout;

#[test]
fn parallel_checkout_prepare_leaves_empty_worktree_then_materializes() {
    let git = resolve_binary("git", None).expect("system git");
    let bare = ensure_bare_remote(&git, ParallelProfile::Smoke).expect("bare smoke fixture");
    let url = file_url(&bare).expect("file url");
    let dest = parallel_clone_dir();

    prepare_parallel_checkout(&git, &url, &dest).expect("prepare clone --no-checkout");
    assert_clone_no_checkout_worktree(&dest).expect("empty after prepare");
    assert_eq!(
        worktree_non_git_path_count(&dest).expect("count"),
        0,
        "worktree must have zero non-.git paths before checkout"
    );

    let status = Command::new(&git)
        .args(["-C", dest.to_str().expect("utf8"), "checkout", "-f", "HEAD"])
        .status()
        .expect("git checkout");
    assert!(status.success(), "git checkout -f HEAD");

    let after = worktree_non_git_path_count(&dest).expect("count after");
    assert!(
        after > 0,
        "checkout should materialize tracked files (smoke fixture has {})",
        ParallelProfile::Smoke.file_count()
    );
}
