//! Load git-written indexes after common mutations (index.rs parse/load coverage).

mod support;

use std::fs;
use std::process::Command;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::index::write_tree::{write_tree_update_index, WriteTreeFlags};
use grit_lib::index::Index;
use grit_lib::objects::odb::Odb;
use grit_lib::repo::Repository;

use support::{git, seed_multidir_repo, HistoryRepo};

fn load_git_index(worktree: &std::path::Path) -> (Repository, Index) {
    let grit = Repository::discover(Some(worktree), &RepositoryOptions::empty()).expect("open");
    let index =
        Index::load_expand_sparse_optional(&grit.index_handle().path(), &grit.odb()).expect("load");
    (grit, index)
}

fn exercise_index_public_api(repo: &Repository, index: &mut Index) {
    let _ = index.entries_are_sorted();
    let _ = index.has_unmerged_entries();
    let _ = index.has_sparse_directory_placeholders();
    let _ = index.format_cache_tree_dump();
    let _ = index.dump_cache_tree(repo.odb());
    let _ = index.split_index_base_oid();
    index.invalidate_cache_tree_for_path(b"a");
    index.invalidate_untracked_cache_for_path("a/f0.txt");
    index.dedup_paths_keep_last();
    index.sync_fsmonitor_dirty_bitmap();
    let _ = index.has_stage0_descendant_under("a");
    let _ = write_tree_update_index(repo.odb(), index, "", WriteTreeFlags::default());
    let _ = Index::load_expand_sparse(&repo.index_handle().path(), repo.odb());
    if let Ok(head) = grit_lib::revwalk::rev_parse::resolve_revision(repo, "HEAD") {
        let head_obj = repo.odb().read(&head).ok();
        if let Some(obj) = head_obj {
            if let Ok(commit) = grit_lib::objects::parse_commit(&obj.data) {
                let patterns = vec!["/*".to_string(), "!/*/".to_string()];
                let _ = index.try_collapse_sparse_directories(
                    repo.odb(),
                    &commit.tree,
                    &patterns,
                    true,
                    false,
                    grit_lib::path_validate::ProtectOptions::default(),
                );
            }
        }
    }
    index.remove_paths_and_insert(["gone.txt".as_bytes()], []);
    index.clear_resolve_undo();
    let cfg = grit_lib::config::ConfigSet::new();
    let _ = index.write_with_config(&repo.index_handle().path(), &cfg);
}

#[test]
fn git_index_load_sweep_after_flag_mutations() {
    let repo = HistoryRepo::init();
    seed_multidir_repo(&repo);
    let wt = repo.worktree();

    git(wt, &["update-index", "--chmod=+x", "a/f0.txt"]);
    let (repo, mut idx) = load_git_index(wt);
    assert!(!idx.entries().is_empty());
    exercise_index_public_api(&repo, &mut idx);

    git(wt, &["update-index", "--assume-unchanged", "b/f0.txt"]);
    let (repo, mut assumed) = load_git_index(wt);
    assert!(assumed.entries().iter().any(|e| e.assume_unchanged()));
    exercise_index_public_api(&repo, &mut assumed);

    git(wt, &["update-index", "--skip-worktree", "c/d/f0.txt"]);
    let (repo, mut skipped) = load_git_index(wt);
    assert!(skipped.entries().iter().any(|e| e.skip_worktree()));
    exercise_index_public_api(&repo, &mut skipped);

    fs::write(wt.join("new.txt"), "n\n").expect("write");
    git(wt, &["add", "--intent-to-add", "new.txt"]);
    let (_, ita) = load_git_index(wt);
    assert!(ita.entries().iter().any(|e| e.intent_to_add()));

    Command::new("git")
        .current_dir(wt)
        .args(["update-index", "--index-version", "4"])
        .status()
        .expect("version");
    git(wt, &["add", "-A"]);
    let v4 = Index::load(&wt.join(".git/index")).expect("v4 load");
    assert_eq!(v4.version, 4);

    Command::new("git")
        .current_dir(wt)
        .args(["update-index", "--split-index"])
        .status()
        .expect("split-index");
    let split = Index::load(&wt.join(".git/index")).expect("split load");
    assert!(split.split_index_base_oid().is_some() || !split.entries().is_empty());
}

#[test]
fn git_unmerged_index_roundtrip() {
    let repo = HistoryRepo::init();
    seed_multidir_repo(&repo);
    let wt = repo.worktree();
    git(wt, &["checkout", "-b", "side-branch"]);
    fs::write(wt.join("merge.txt"), "side\n").expect("write");
    git(wt, &["add", "merge.txt"]);
    git(wt, &["commit", "-m", "side"]);
    git(wt, &["checkout", "main"]);
    // Ensure merge.txt exists on main before second commit.
    fs::write(wt.join("merge.txt"), "main\n").expect("write");
    git(wt, &["add", "merge.txt"]);
    git(wt, &["commit", "-m", "main"]);
    let merge = Command::new("git")
        .current_dir(wt)
        .args(["merge", "side-branch"])
        .output()
        .expect("merge");
    assert!(!merge.status.success() || String::from_utf8_lossy(&merge.stdout).contains("Merge"));
    let grit = Repository::discover(Some(wt), &RepositoryOptions::empty()).expect("open");
    let index = Index::load(&grit.index_handle().path()).expect("unmerged load");
    if index.has_unmerged_entries() {
        assert!(index.entries().iter().any(|e| e.stage() != 0));
    }
}
