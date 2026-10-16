//! `grit add` and index write compatibility with system Git (v2/v3/v4, cache-tree).

mod support;

use std::fs;
use std::process::Command;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::index::write_tree::{write_tree_update_index, WriteTreeFlags};
use grit_lib::index::Index;
use grit_lib::objects::ObjectId;
use grit_lib::porcelain::add::{stage, StageMode, StageOptions};
use grit_lib::progress::NullProgress;
use grit_lib::repo::Repository;

use support::{
    assert_git_fsck_strict, assert_grit_index_matches_git_ls_files, git, git_ls_files_stage,
    seed_multidir_repo, HistoryRepo,
};

fn open_grit(wt: &std::path::Path) -> Repository {
    Repository::discover(Some(wt), &RepositoryOptions::empty()).expect("open grit repo")
}

#[test]
fn grit_written_index_v2_v3_v4_accepted_by_git() {
    for version in [2u32, 3, 4] {
        let repo = HistoryRepo::init();
        seed_multidir_repo(&repo);
        let wt = repo.worktree();

        let grit = open_grit(wt);
        let mut index = grit.index_handle().load().expect("load");
        index.version = version;
        if version == 3 {
            if let Some(e) = index.entries_mut().first_mut() {
                e.set_skip_worktree(true);
            }
        }
        grit.index_handle().write(&mut index).expect("grit write");
        assert_git_fsck_strict(wt);
        assert_grit_index_matches_git_ls_files(wt, &index);
    }
}

#[test]
fn git_written_index_reload_byte_identical_when_unchanged() {
    let repo = HistoryRepo::init();
    seed_multidir_repo(&repo);
    let wt = repo.worktree();
    git(wt, &["add", "-A"]);
    let bytes = fs::read(wt.join(".git/index")).expect("git index");

    let grit = open_grit(wt);
    let mut parsed = Index::parse_with_algo(&bytes, grit.odb().hash_algo()).expect("parse");
    grit.index_handle().write(&mut parsed).expect("rewrite");
    let round = fs::read(wt.join(".git/index")).expect("roundtrip index");
    assert_eq!(
        bytes, round,
        "unchanged logical index should round-trip bytes"
    );
}

#[test]
fn cache_tree_after_add_matches_git_write_tree() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo_dir = tmp.path().join("repo");
    fs::create_dir_all(&repo_dir).unwrap();
    git(&repo_dir, &["init", "-q", "-b", "main", "."]);

    for d in 0..3 {
        let dir = repo_dir.join(format!("d{d}"));
        fs::create_dir_all(&dir).unwrap();
        for f in 0..20 {
            fs::write(dir.join(format!("f{f}.txt")), format!("{d}/{f}\n")).unwrap();
        }
    }
    git(&repo_dir, &["add", "-A"]);
    git(&repo_dir, &["commit", "-q", "-m", "initial"]);

    fs::write(repo_dir.join("d1/f5.txt"), "changed\n").unwrap();

    let grit = open_grit(&repo_dir);
    let opts = StageOptions::default();
    stage(&grit, &opts, &mut NullProgress).expect("grit add");

    let git_tree_hex = String::from_utf8(
        Command::new("git")
            .current_dir(&repo_dir)
            .args(["write-tree"])
            .output()
            .expect("git write-tree")
            .stdout,
    )
    .expect("utf8")
    .trim()
    .to_owned();
    let git_tree = ObjectId::from_hex(&git_tree_hex).expect("parse git tree");

    let mut index = grit.index_handle().load().expect("load after add");
    let grit_tree = write_tree_update_index(grit.odb(), &mut index, "", WriteTreeFlags::default())
        .expect("grit write-tree from index");
    assert_eq!(grit_tree, git_tree);

    let grit_stages = git_ls_files_stage(&repo_dir);
    assert_grit_index_matches_git_ls_files(&repo_dir, &index);
    assert!(!grit_stages.is_empty());
    assert_git_fsck_strict(&repo_dir);
}

#[test]
fn add_untracked_path_keeps_index_entries_canonical() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo_dir = tmp.path().join("repo");
    fs::create_dir_all(&repo_dir).unwrap();
    git(&repo_dir, &["init", "-q", "-b", "main", "."]);
    fs::write(repo_dir.join("zzz.txt"), "z\n").unwrap();
    git(&repo_dir, &["add", "zzz.txt"]);
    git(&repo_dir, &["commit", "-q", "-m", "initial"]);

    fs::write(repo_dir.join("aaa.txt"), "a\n").unwrap();

    let grit = open_grit(&repo_dir);
    let opts = StageOptions::default();
    stage(&grit, &opts, &mut NullProgress).expect("grit add");

    assert_git_fsck_strict(&repo_dir);
    let index = grit.index_handle().load().expect("load index");
    assert!(
        index.entries_are_sorted(),
        "index paths must stay sorted for git fsck"
    );
    assert_eq!(
        index.entries().first().map(|e| e.path.as_slice()),
        Some(b"aaa.txt" as &[u8])
    );
}
