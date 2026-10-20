//! Targeted git.git rename boundary checks (requires `/tmp/git-wt` full clone).

mod support;

use grit_lib::config::ConfigSet;
use grit_lib::diff::{detect_renames_with_policy, diff_trees, DiffStatus};
use grit_lib::objects::ObjectId;
use grit_lib::parallel::ThreadPolicy;
use std::path::Path;

use support::open_grit_repo;

#[test]
fn rename_builtin_add_detected_at_subdirectory_move() {
    let wt = Path::new("/tmp/git-wt");
    if !wt.is_dir() {
        return;
    }
    let repo = open_grit_repo(wt);
    let parent_tree: ObjectId = "73c6c9da005ffbd2abd2b05f92347fd5c7b0796d".parse().unwrap();
    let commit_tree: ObjectId = "7e86bb81e83c9fad73dcbdaa0ef33038137b4274".parse().unwrap();
    let config = ConfigSet::new();
    let policy = ThreadPolicy::default();
    let entries = diff_trees(repo.odb(), Some(&parent_tree), Some(&commit_tree), "").unwrap();
    let renamed_root = detect_renames_with_policy(
        repo.odb(),
        None,
        entries,
        50,
        &config,
        &policy,
    )
    .into_iter()
    .find(|e| {
        e.status == DiffStatus::Renamed && e.new_path.as_deref() == Some("builtin/add.c")
    });
    assert!(
        renamed_root.is_some(),
        "expected root diff to find builtin-add.c -> builtin/add.c rename"
    );
}
