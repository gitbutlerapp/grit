//! Regression for issue #1001: linked worktree remove after grit commit.
//!
//! Linux CI cannot reproduce the Windows autocrlf stale-stat bug from `create_commit`
//! alone, so this module uses a deterministic stale-stat fixture (wrong cached size with
//! matching OID). `git diff-files` reports such an entry until the index stat is refreshed.
//! Grit decides by content under `core.autocrlf` (issue #2164): a stale size whose cleaned
//! file is still the indexed blob does not block remove, while a real change does.
//! On Windows, an additional test exercises the real post-commit path.

use grit_lib::progress::NullProgress;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::diff::{refresh_index_stat_content_verified_with_rules, stat_matches};
use grit_lib::index::write_tree::{write_tree_update_index, WriteTreeFlags};
use grit_lib::index::IndexEntry;
use grit_lib::objects::{serialize_commit, CommitData, ObjectKind};
use grit_lib::porcelain::add::StageOptions;
use grit_lib::porcelain::commit::CommitRequest;
use grit_lib::progress::NullProgress;
use grit_lib::refs::store::RefNameRef;
use grit_lib::refs::{update_branch_for_commit_with_config, BranchCommitRefUpdate};
use grit_lib::repo::Repository;
use grit_lib::worktree::worktree_rules::WorktreeRules;
use grit_lib::worktree::{AddLinkedWorktreeOptions, WorktreeBranch};
use grit_test_support::git;

fn commit_request(message: &str) -> CommitRequest {
    {
        let mut req = CommitRequest::new(message.to_owned());
        req.author = Some(
            "Test <t@example.com> 1000000000 +0000"
                .parse()
                .expect("signature"),
        );
        req.committer = Some(
            "Test <t@example.com> 1000000000 +0000"
                .parse()
                .expect("signature"),
        );
        req.allow_empty = false;
        req.sign_override = None;
        req.amend = false;
        req.assume_worktree_synced = false;
        req
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
    main_repo
        .worktrees()
        .add(&AddLinkedWorktreeOptions {
            path: linked.clone(),
            branch: Some(WorktreeBranch::Create("linked".to_owned())),
            start_point: Some("HEAD".to_owned()),
            force: false,
            lock: None,
            no_checkout: false,
            track: false,
        })
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
    let mut index = repo
        .index()
        .load(&grit_lib::index::IndexLoadOptions::default())
        .expect("load index");
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
    repo.index()
        .write(&mut index, &grit_lib::index::IndexWriteOptions::default())
        .expect("write stale index");
}

fn refresh_index_stat_on_disk(repo: &Repository) {
    let work_tree = repo.work_tree().expect("work tree");
    let mut index = repo
        .index()
        .load(&grit_lib::index::IndexLoadOptions::default())
        .expect("load index");
    let rules = WorktreeRules::from_repository(repo, &index).ok();
    let rules_arc = rules.map(|r| Arc::new(Mutex::new(r)));
    let config = repo.config().snapshot().ok();
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
    repo.index()
        .write(&mut index, &grit_lib::index::IndexWriteOptions::default())
        .expect("write refreshed index");
}

/// Persist a commit after write-tree **without** the stat refresh `create_commit` performs.
fn commit_staged_without_index_stat_refresh(
    repo: &Repository,
    message: &str,
) -> grit_lib::objects::ObjectId {
    let head = repo.references().head().expect("head");
    let (refname, _short, parent) = match head {
        grit_lib::state::HeadState::Branch {
            refname,
            short_name,
            oid,
        } => (refname, short_name, oid),
        _ => panic!("expected branch head"),
    };
    let mut index = repo
        .index()
        .load(&grit_lib::index::IndexLoadOptions::default())
        .expect("index");
    let tree = write_tree_update_index(repo.odb(), &mut index, "", WriteTreeFlags::silent())
        .expect("write tree");
    let commit_data = CommitData::new(
        tree,
        parent.into_iter().collect(),
        "Test <t@example.com> 1000000000 +0000",
        "Test <t@example.com> 1000000000 +0000",
        format!("{message}\n"),
    );
    let bytes = serialize_commit(&commit_data);
    let oid = repo
        .odb()
        .write(ObjectKind::Commit, &bytes)
        .expect("write commit");
    repo.index()
        .write(&mut index, &grit_lib::index::IndexWriteOptions::default())
        .expect("write index");
    let config = repo.config().snapshot().expect("config");
    update_branch_for_commit_with_config(
        repo.git_dir(),
        &BranchCommitRefUpdate {
            branch_ref: RefNameRef::from_trusted(refname.as_str()),
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
    let index = repo
        .index()
        .load(&grit_lib::index::IndexLoadOptions::default())
        .expect("index");
    let path_bytes = rel_path.as_bytes();
    index
        .entries()
        .iter()
        .find(|e| e.stage() == 0 && e.path.as_slice() == path_bytes)
        .cloned()
        .unwrap_or_else(|| panic!("no index entry for {rel_path}"))
}

/// Linked worktree whose committed `new.txt` has a stale cached size (same blob, `mtime` 0).
fn stale_index_stat_fixture() -> (tempfile::TempDir, PathBuf, Repository, Repository) {
    let (tmp, _main, linked, main_repo) = linked_worktree_fixture();
    let linked_repo =
        Repository::discover(Some(linked.as_path()), &RepositoryOptions::empty()).expect("linked");
    fs::write(linked.join("new.txt"), "new\n").expect("new file");
    linked_repo
        .index()
        .stage(
            &StageOptions::default(),
            &mut NullProgress,
            &grit_lib::progress::CancelToken::never(),
        )
        .expect("stage");
    commit_staged_without_index_stat_refresh(&linked_repo, "no refresh");

    let entry = index_entry(&linked_repo, "new.txt");
    let wrong_size = entry.size.saturating_add(1);
    apply_stale_index_stat_size_mismatch(&linked_repo, "new.txt", wrong_size);
    assert!(
        !git_diff_files_quiet(&linked),
        "git diff-files must detect stale cached stat size"
    );
    (tmp, linked, linked_repo, main_repo)
}

#[test]
fn stale_index_stat_with_identical_content_allows_worktree_remove() {
    let (_tmp, linked, _linked_repo, main_repo) = stale_index_stat_fixture();
    main_repo
        .worktrees()
        .remove(&linked, false)
        .expect("a stale size whose cleaned content is the indexed blob loses no data");
    assert!(!linked.exists(), "linked worktree directory removed");
}

#[test]
fn stale_index_stat_refresh_clears_git_diff_files_before_remove() {
    let (_tmp, linked, linked_repo, main_repo) = stale_index_stat_fixture();
    refresh_index_stat_on_disk(&linked_repo);
    assert!(
        git_diff_files_quiet(&linked),
        "refresh must align index stat with worktree for git diff-files"
    );
    main_repo
        .worktrees()
        .remove(&linked, false)
        .expect("remove after refresh");
}

#[test]
fn stale_index_stat_with_changed_content_blocks_worktree_remove() {
    let (_tmp, linked, _linked_repo, main_repo) = stale_index_stat_fixture();
    fs::write(linked.join("new.txt"), "changed\n").expect("real edit");
    assert!(
        !git_diff_files_quiet(&linked),
        "git diff-files must report the edit"
    );
    assert!(
        main_repo.worktrees().remove(&linked, false).is_err(),
        "remove must refuse a worktree whose content changed under core.autocrlf"
    );
    assert!(
        linked.join("new.txt").exists(),
        "refused remove keeps the edit"
    );
}

#[test]
fn create_commit_refreshes_index_stat_for_worktree_remove() {
    let (_tmp, _main, linked, main_repo) = linked_worktree_fixture();
    let linked_repo =
        Repository::discover(Some(linked.as_path()), &RepositoryOptions::empty()).expect("linked");
    fs::write(linked.join("new.txt"), "new\n").expect("new file");
    linked_repo
        .index()
        .stage(
            &StageOptions::default(),
            &mut NullProgress,
            &grit_lib::progress::CancelToken::never(),
        )
        .expect("stage");
    // Bump mtime after staging so write-tree without stat refresh would persist stale stat.
    let new_path = linked.join("new.txt");
    let bumped = filetime::FileTime::from_unix_time(1_700_000_000, 0);
    filetime::set_file_mtime(&new_path, bumped).expect("touch new.txt");
    linked_repo
        .commits()
        .create_staged(
            &commit_request("with refresh"),
            &mut NullProgress,
            &grit_lib::progress::CancelToken::never(),
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
    main_repo
        .worktrees()
        .remove(&linked, false)
        .expect("remove after create_commit");
}

#[cfg(windows)]
#[test]
fn create_commit_in_linked_worktree_allows_remove_on_windows() {
    let (_tmp, _main, linked, main_repo) = linked_worktree_fixture();
    let linked_repo =
        Repository::discover(Some(linked.as_path()), &RepositoryOptions::empty()).expect("linked");
    fs::write(linked.join("new.txt"), "new\n").expect("new file");
    linked_repo
        .index()
        .stage(&StageOptions::default(), &mut NullProgress)
        .expect("stage");
    linked_repo
        .commits()
        .create_staged(&commit_request("windows path"), &mut NullProgress)
        .expect("create_commit");
    assert!(
        git_diff_files_quiet(&linked),
        "Windows repro: git diff-files must be clean without update-index --refresh"
    );
    main_repo
        .worktrees()
        .remove(&linked, false)
        .expect("remove without prior status");
}
