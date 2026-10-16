//! Skip-worktree and assume-unchanged bits vs system git (t2104/t7011/t7012).

mod support;

use std::fs;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::index::sparse_checkout::apply_sparse_checkout_skip_worktree;
use grit_lib::index::Index;
use grit_lib::objects::ObjectKind;
use grit_lib::repo::Repository;

use support::{
    assert_git_fsck_strict, assert_porcelain_v2_matches_git,
    assert_skip_worktree_matches_tag_oracle, clear_all_skip_worktree, git, git_ls_files_tag_map,
    git_ls_files_verbose_map, seed_multidir_repo, HistoryRepo,
};

fn open_grit(wt: &std::path::Path) -> Repository {
    Repository::discover(Some(wt), &RepositoryOptions::empty()).expect("open")
}

#[test]
fn t7012_skip_worktree_bits_match_git_ls_files_v() {
    if !support::skip_unless_git_at_least(2, 25) {
        return;
    }
    let repo = HistoryRepo::init();
    seed_multidir_repo(&repo);
    let wt = repo.worktree();
    git(wt, &["sparse-checkout", "init", "--cone"]);
    git(wt, &["sparse-checkout", "set", "a"]);
    git(wt, &["read-tree", "-mu", "HEAD"]);

    let oracle = git_ls_files_tag_map(wt);
    assert!(
        oracle.values().any(|t| *t == 'S'),
        "git must mark sparse-excluded paths skip-worktree"
    );

    let grit = open_grit(wt);
    let mut index = grit.index_handle().load().expect("load");
    clear_all_skip_worktree(&mut index);
    assert!(
        index.entries().iter().all(|e| !e.skip_worktree()),
        "cleared index must not yet match sparse cone"
    );
    apply_sparse_checkout_skip_worktree(&grit.git_dir(), Some(wt), &mut index, false);
    assert_skip_worktree_matches_tag_oracle(&index, &oracle);
    grit.index_handle().write(&mut index).expect("write");
    assert_git_fsck_strict(wt);
    assert_skip_worktree_matches_tag_oracle(&index, &oracle);
}

#[test]
fn t7011_assume_unchanged_seen_by_git_ls_files_v() {
    let repo = HistoryRepo::init();
    seed_multidir_repo(&repo);
    let wt = repo.worktree();
    let grit = open_grit(wt);
    let mut index = grit.index_handle().load().expect("load");
    for entry in index.entries_mut() {
        entry.set_assume_unchanged(false);
    }
    let entry = index
        .entries_mut()
        .iter_mut()
        .find(|e| e.path.ends_with(b"a/f0.txt"))
        .expect("entry");
    entry.set_assume_unchanged(true);
    grit.index_handle().write(&mut index).expect("write");
    assert_git_fsck_strict(wt);

    let tags = git_ls_files_verbose_map(wt);
    assert_eq!(tags.get("a/f0.txt").copied(), Some('h'));
}

#[test]
fn t2104_skip_worktree_excluded_from_status_like_git() {
    if !support::skip_unless_git_at_least(2, 25) {
        return;
    }
    let repo = HistoryRepo::init();
    seed_multidir_repo(&repo);
    let wt = repo.worktree();
    git(wt, &["sparse-checkout", "init", "--cone"]);
    git(wt, &["sparse-checkout", "set", "a"]);
    git(wt, &["read-tree", "-mu", "HEAD"]);

    let grit = open_grit(wt);
    let mut index = grit.index_handle().load().expect("load");
    clear_all_skip_worktree(&mut index);
    apply_sparse_checkout_skip_worktree(&grit.git_dir(), Some(wt), &mut index, false);

    fs::write(wt.join("a/f0.txt"), "changed\n").expect("touch included");
    let blob = grit
        .odb()
        .write(ObjectKind::Blob, b"changed excluded\n")
        .expect("blob");
    let excluded = index
        .get_mut(b"b/f0.txt", 0)
        .expect("excluded path in index");
    assert!(
        excluded.skip_worktree(),
        "excluded path must carry skip-worktree before index-only mutation"
    );
    excluded.oid = blob;

    grit.index_handle().write(&mut index).expect("write");
    assert_porcelain_v2_matches_git(wt, &grit);
}
