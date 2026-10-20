//! Sparse-checkout cone/non-cone vs git ls-files -t/-v (t1091/t1092/t1011).

mod support;

use std::fs;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::index::sparse_checkout::apply_sparse_checkout_skip_worktree;
use grit_lib::repo::Repository;

use support::{
    assert_git_fsck_strict, assert_skip_worktree_matches_tag_oracle, clear_all_skip_worktree, git,
    git_ls_files_tag_map, git_ls_files_verbose_map, seed_multidir_repo, HistoryRepo,
};

fn open_grit(wt: &std::path::Path) -> Repository {
    Repository::discover(Some(wt), &RepositoryOptions::empty()).expect("open")
}

#[test]
fn t1092_cone_patterns_match_git_skip_worktree() {
    if !support::skip_unless_git_at_least(2, 25) {
        return;
    }
    let repo = HistoryRepo::init();
    seed_multidir_repo(&repo);
    let wt = repo.worktree();
    git(wt, &["sparse-checkout", "init", "--cone"]);
    git(wt, &["sparse-checkout", "set", "a", "c"]);
    git(wt, &["read-tree", "-mu", "HEAD"]);

    let oracle = git_ls_files_tag_map(wt);
    let grit = open_grit(wt);
    let mut index = grit.index_handle().load().expect("load");
    clear_all_skip_worktree(&mut index);
    apply_sparse_checkout_skip_worktree(&grit.git_dir(), Some(wt), &mut index, false);
    assert_skip_worktree_matches_tag_oracle(&index, &oracle);
    grit.index_handle().write(&mut index).expect("write");
    assert_git_fsck_strict(wt);
    assert_skip_worktree_matches_tag_oracle(&index, &oracle);
}

#[test]
fn t1091_non_cone_sparse_list_matches_git() {
    if !support::skip_unless_git_at_least(2, 25) {
        return;
    }
    let repo = HistoryRepo::init();
    seed_multidir_repo(&repo);
    let wt = repo.worktree();
    git(wt, &["sparse-checkout", "init", "--no-cone"]);
    git(wt, &["sparse-checkout", "set", "/*", "!/*/", "a/"]);
    git(wt, &["read-tree", "-mu", "HEAD"]);

    let grit = open_grit(wt);
    let list = git(wt, &["sparse-checkout", "list"]);
    let file = fs::read_to_string(wt.join(".git/info/sparse-checkout")).expect("read");
    assert_eq!(list.trim(), file.trim());

    let oracle = git_ls_files_tag_map(wt);
    let mut index = grit.index_handle().load().expect("load");
    clear_all_skip_worktree(&mut index);
    apply_sparse_checkout_skip_worktree(&grit.git_dir(), Some(wt), &mut index, false);
    assert_skip_worktree_matches_tag_oracle(&index, &oracle);
}

#[test]
fn t1011_read_tree_sparse_checkout_applies_skip_worktree() {
    if !support::skip_unless_git_at_least(2, 25) {
        return;
    }
    let repo = HistoryRepo::init();
    seed_multidir_repo(&repo);
    let wt = repo.worktree();
    git(wt, &["sparse-checkout", "init", "--cone"]);
    git(wt, &["sparse-checkout", "set", "b"]);
    git(wt, &["read-tree", "-mu", "HEAD"]);

    let oracle = git_ls_files_tag_map(wt);
    assert!(oracle.values().any(|c| *c == 'S'));
    let grit = open_grit(wt);
    let mut index = grit.index_handle().load().expect("load");
    clear_all_skip_worktree(&mut index);
    apply_sparse_checkout_skip_worktree(&grit.git_dir(), Some(wt), &mut index, false);
    assert_skip_worktree_matches_tag_oracle(&index, &oracle);
}

#[test]
fn git_written_sparse_index_sdir_load_or_typed_error() {
    if !support::skip_unless_git_at_least(2, 37) {
        return;
    }
    let repo = HistoryRepo::init();
    seed_multidir_repo(&repo);
    let wt = repo.worktree();
    git(wt, &["sparse-checkout", "init", "--cone", "--sparse-index"]);
    git(wt, &["sparse-checkout", "set", "a"]);
    git(wt, &["read-tree", "-mu", "HEAD"]);

    let index_path = wt.join(".git/index");
    let bytes = fs::read(&index_path).expect("read");
    let has_sdir = bytes.windows(4).any(|w| w == b"sdir");
    if !has_sdir {
        eprintln!("SKIP: git did not write sdir extension on this platform");
        return;
    }

    let _grit = open_grit(wt);
    let load_result = grit_lib::index::Index::load(&index_path);
    match load_result {
        Ok(idx) => {
            assert!(idx.sparse_directories || idx.has_sparse_directory_placeholders());
        }
        Err(e) => {
            assert!(
                matches!(e, grit_lib::error::Error::IndexError(_)),
                "unexpected error type: {e:?}"
            );
        }
    }
}
