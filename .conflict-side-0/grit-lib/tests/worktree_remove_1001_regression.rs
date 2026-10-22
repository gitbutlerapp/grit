//! Regression for issue #1001: linked worktree remove after grit commit.
//!
//! Linux CI cannot reproduce the Windows autocrlf stale-stat bug from `create_commit`
//! alone, so this module uses a deterministic stale-stat fixture (wrong cached size with
//! matching OID) to assert remove and `git diff-files` fail until index stat refresh runs.
//! On Windows, an additional test exercises the real post-commit path.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::diff::{refresh_index_stat_content_verified_with_rules, stat_matches};
use grit_lib::index::write_tree::{write_tree_update_index, WriteTreeFlags};
use grit_lib::index::IndexEntry;
use grit_lib::objects::{serialize_commit, CommitData, ObjectKind};
use grit_lib::porcelain::add::{stage, StageOptions};
use grit_lib::porcelain::commit::{create_commit, CommitRequest, HookPolicy};
use grit_lib::progress::NullProgress;
use grit_lib::refs::{update_branch_for_commit_with_config, BranchCommitRefUpdate};
use grit_lib::repo::Repository;
use grit_lib::worktree::worktree_rules::WorktreeRules;
use grit_lib::worktree::{add_linked_worktree, remove_linked_worktree, AddLinkedWorktreeOptions};
use grit_test_support::git;

fn commit_request(message: &str) -> CommitRequest {
    CommitRequest {
        message: message.to_owned(),
        author: "Test <t@example.com> 1000000000 +0000".into(),
        committer: "Test <t@example.com> 1000000000 +0000".into(),
        allow_empty: false,
        sign_override: None,
        amend: false,
        assume_worktree_synced: false,
        skip_pre_commit_tree_match: false,
        staged_change_count: None,
        ..Default::default()
    }
}

fn init_main_repo(main: &Path) {
    fs::create_dir_all(main).expect("mkdir main");
    git(main, &["init", "-q", "-b", "main"]);
    git(main, &["config", "user.email", "t@example.com"]);
    git(main, &["config", "user.name", "Test"]);
    git(main, &["config", "core.autocrlf", "true"]);
    fs::write(main.join("tracked.txt"), "1\n").expect("tracked");
    git(main, &["add", "tracked.txt"]);
    git(main, &["commit", "-qm", "seed"]);
}

fn linked_worktree_fixture() -> (tempfile::TempDir, PathBuf, PathBuf, Repository) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let main = tmp.path().join("main");
    init_main_repo(&main);
    let main_repo =
        Repository::discover(Some(main.as_path()), &RepositoryOptions::empty()).expect("main");
    let linked = tmp.path().join("linked");
    add_linked_worktree(
        &main_repo,
        &AddLinkedWorktreeOptions {
            path: linked.clone(),
            branch: Some("linked".to_owned()),
            new_branch: true,
            detach: false,
            start_point: Some("HEAD".to_owned()),
            force: false,
        },
    )
    .expect("add linked");
    (tmp, main, linked, main_repo)
}

fn git_diff_files_quiet(worktree: &Path) -> bool {
    Command::new("git")
        .args(["diff-files", "--quiet"])
        .current_dir(worktree)
        .status()
        .expect("git diff-files")
        .success()
}

/// Issue #1001 on Windows: index OID matches the worktree but cached stat size is stale
/// after write-tree commit persistence. Force the same shape on every platform.
fn apply_stale_index_stat_size_mismatch(repo: &Repository, rel_path: &str, wrong_size: u32) {
    let mut index = repo.index_handle().load().expect("load index");
    let path_bytes = rel_path.as_bytes();
    let mut found = false;
    for entry in index.entries_mut() {
        if entry.path.as_slice() == path_bytes && entry.stage() == 0 {
            entry.size = wrong_size;
            entry.mtime_sec = 0;
            entry.mtime_nsec = 0;
            found = true;
        }
    }
    assert!(found, "missing index entry for {rel_path}");
    repo.index_handle()
        .write(&mut index)
        .expect("write stale index");
}

fn refresh_index_stat_on_disk(repo: &Repository) {
    let work_tree = repo.work_tree().expect("work tree");
    let mut index = repo.index_handle().load().expect("load index");
    let rules = WorktreeRules::from_repository(repo, &index).ok();
    let rules_arc = rules.map(|r| Arc::new(Mutex::new(r)));
    let config = repo.config().ok();
    refresh_index_stat_content_verified_with_rules(
        repo.odb(),
        repo.git_dir(),
        &mut index,
        work_tree,
        None,
        config.as_deref(),
        None,
        rules_arc.as_ref(),
    )
    .expect("refresh index stat");
    repo.index_handle()
        .write(&mut index)
        .expect("write refreshed index");
}

/// Persist a commit after write-tree **without** the stat refresh `create_commit` performs.
fn commit_staged_without_index_stat_refresh(
    repo: &Repository,
    message: &str,
) -> grit_lib::objects::ObjectId {
    let head = grit_lib::state::resolve_head(repo.git_dir()).expect("head");
    let (refname, _short, parent) = match head {
        grit_lib::state::HeadState::Branch {
            refname,
            short_name,
            oid,
        } => (refname, short_name, oid),
        _ => panic!("expected branch head"),
    };
    let mut index = repo.index_handle().load().expect("index");
    let tree = write_tree_update_index(repo.odb(), &mut index, "", WriteTreeFlags::silent())
        .expect("write tree");
    let commit_data = CommitData {
        tree,
        parents: parent.into_iter().collect(),
        author: "Test <t@example.com> 1000000000 +0000".into(),
        committer: "Test <t@example.com> 1000000000 +0000".into(),
        author_raw: Vec::new(),
        committer_raw: Vec::new(),
        encoding: None,
        message: format!("{message}\n"),
        raw_message: None,
        extra_headers: Vec::new(),
    };
    let bytes = serialize_commit(&commit_data);
    let oid = repo
        .odb()
        .write(ObjectKind::Commit, &bytes)
        .expect("write commit");
    repo.index_handle().write(&mut index).expect("write index");
    let config = repo.config().expect("config");
    update_branch_for_commit_with_config(
        repo.git_dir(),
        &BranchCommitRefUpdate {
            branch_ref: &refname,
            expected_old: parent,
            new_oid: oid,
            identity: &commit_data.committer,
            reflog_message: "commit: test",
        },
        config.as_ref(),
    )
    .expect("update ref");
    oid
}

fn index_entry(repo: &Repository, rel_path: &str) -> IndexEntry {
    let index = repo.index_handle().load().expect("index");
    let path_bytes = rel_path.as_bytes();
    index
        .entries()
        .iter()
        .find(|e| e.stage() == 0 && e.path.as_slice() == path_bytes)
        .cloned()
        .unwrap_or_else(|| panic!("no index entry for {rel_path}"))
}

#[test]
fn stale_index_stat_fixture_blocks_worktree_remove_until_refresh() {
    let (_tmp, _main, linked, main_repo) = linked_worktree_fixture();
    let linked_repo =
        Repository::discover(Some(linked.as_path()), &RepositoryOptions::empty()).expect("linked");
    fs::write(linked.join("new.txt"), "new\n").expect("new file");
    stage(&linked_repo, &StageOptions::default(), &mut NullProgress).expect("stage");
    commit_staged_without_index_stat_refresh(&linked_repo, "no refresh");

    let entry = index_entry(&linked_repo, "new.txt");
    let wrong_size = entry.size.saturating_add(1);
    apply_stale_index_stat_size_mismatch(&linked_repo, "new.txt", wrong_size);

    assert!(
        !git_diff_files_quiet(&linked),
        "git diff-files must detect stale cached stat size"
    );
    assert!(
        remove_linked_worktree(&main_repo, &linked, false).is_err(),
        "remove must refuse a worktree when git sees index/worktree stat mismatch"
    );

    refresh_index_stat_on_disk(&linked_repo);
    assert!(
        git_diff_files_quiet(&linked),
        "refresh must align index stat with worktree for git diff-files"
    );
    remove_linked_worktree(&main_repo, &linked, false).expect("remove after refresh");
}

#[test]
fn create_commit_refreshes_index_stat_for_worktree_remove() {
    let (_tmp, _main, linked, main_repo) = linked_worktree_fixture();
    let linked_repo =
        Repository::discover(Some(linked.as_path()), &RepositoryOptions::empty()).expect("linked");
    fs::write(linked.join("new.txt"), "new\n").expect("new file");
    stage(&linked_repo, &StageOptions::default(), &mut NullProgress).expect("stage");
    // Bump mtime after staging so write-tree without stat refresh would persist stale stat.
    let new_path = linked.join("new.txt");
    let bumped = filetime::FileTime::from_unix_time(1_700_000_000, 0);
    filetime::set_file_mtime(&new_path, bumped).expect("touch new.txt");
    create_commit(
        &linked_repo,
        &commit_request("with refresh"),
        &mut NullProgress,
    )
    .expect("create_commit");

    let work_tree = linked_repo.work_tree().expect("work tree");
    let entry = index_entry(&linked_repo, "new.txt");
    let meta = fs::metadata(work_tree.join("new.txt")).expect("stat new.txt");
    assert!(
        stat_matches(&entry, &meta),
        "create_commit must persist refreshed stat metadata (issue #1001)"
    );
    assert!(
        git_diff_files_quiet(&linked),
        "git diff-files must be clean after create_commit"
    );
    remove_linked_worktree(&main_repo, &linked, false).expect("remove after create_commit");
}

#[cfg(windows)]
#[test]
fn create_commit_in_linked_worktree_allows_remove_on_windows() {
    let (_tmp, _main, linked, main_repo) = linked_worktree_fixture();
    let linked_repo =
        Repository::discover(Some(linked.as_path()), &RepositoryOptions::empty()).expect("linked");
    fs::write(linked.join("new.txt"), "new\n").expect("new file");
    stage(&linked_repo, &StageOptions::default(), &mut NullProgress).expect("stage");
    create_commit(
        &linked_repo,
        &commit_request("windows path"),
        &mut NullProgress,
    )
    .expect("create_commit");
    assert!(
        git_diff_files_quiet(&linked),
        "Windows repro: git diff-files must be clean without update-index --refresh"
    );
    remove_linked_worktree(&main_repo, &linked, false).expect("remove without prior status");
}
