//! Issue #1091: clean merge must not duplicate index entries when the other side renamed paths.

mod support;

use std::collections::HashSet;
use std::fs;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::ObjectId;
use grit_lib::porcelain::commit::{create_commit, CommitRequest};
use grit_lib::porcelain::merge::{integrate_branch, IntegrateBranchOutcome};
use grit_lib::progress::NullProgress;
use grit_lib::repo::Repository;
use grit_test_support::git;

use support::{assert_grit_index_matches_git_ls_files, git_ls_files_stage};

fn commit_req(message: &str) -> CommitRequest {
    let ident = "T <t@e.com> 1700000000 +0000".to_owned();
    CommitRequest {
        message: message.to_owned(),
        author: Some(ident.parse().expect("signature")),
        committer: Some(ident.parse().expect("signature")),
        allow_empty: false,
        sign_override: None,
        amend: false,
        assume_worktree_synced: false,
        skip_pre_commit_tree_match: false,
        staged_change_count: None,
        ..Default::default()
    }
}

fn grit_index_all_stages(index: &grit_lib::index::Index) -> Vec<(String, u32, String, ObjectId)> {
    let mut rows: Vec<(String, u32, String, ObjectId)> = index
        .entries()
        .iter()
        .map(|e| {
            (
                String::from_utf8_lossy(&e.path).into_owned(),
                u32::from(e.stage()),
                format!("{:o}", e.mode),
                e.oid,
            )
        })
        .collect();
    rows.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
    rows
}

fn assert_git_fsck_no_duplicate_entries(worktree: &std::path::Path) {
    use std::process::Command;
    let out = Command::new("git")
        .current_dir(worktree)
        .args(["fsck"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git fsck");
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !combined.contains("duplicateEntries") && !combined.contains("duplicate file entries"),
        "git fsck reported duplicate tree entries: {combined}"
    );
}

fn assert_unique_stage0_paths(repo: &Repository) {
    let index = repo.index_handle().load().expect("index");
    let mut seen = HashSet::new();
    for entry in index.entries() {
        if entry.stage() == 0 {
            let path = String::from_utf8_lossy(&entry.path).into_owned();
            assert!(
                seen.insert(path.clone()),
                "duplicate stage-0 index entry for {path}"
            );
        }
    }
}

fn issue_1091_repo() -> (tempfile::TempDir, Repository) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "user.name", "T"]);
    git(root, &["config", "user.email", "t@e.com"]);

    fs::create_dir_all(root.join("old")).expect("old dir");
    fs::write(root.join("old/f.txt"), "1\n2\n3\n").expect("old/f.txt");
    fs::write(root.join("b.txt"), "hi\n").expect("b.txt");
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "base"]);

    git(root, &["switch", "-q", "-c", "side"]);
    fs::write(root.join("side.txt"), "s\n").expect("side.txt");
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "side"]);

    git(root, &["switch", "-q", "main"]);
    git(root, &["mv", "old", "new"]);
    git(root, &["commit", "-qm", "rename"]);

    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    (tmp, repo)
}

#[test]
fn incoming_directory_rename_does_not_duplicate_merged_index() {
    let (_tmp, repo) = issue_1091_repo();
    let root = repo.work_tree().expect("worktree");

    git(root, &["switch", "-q", "side"]);
    let main_oid = ObjectId::from_hex(git(root, &["rev-parse", "main"]).trim()).expect("main");
    let side_oid = ObjectId::from_hex(git(root, &["rev-parse", "HEAD"]).trim()).expect("side");

    let outcome =
        integrate_branch(&repo, "refs/heads/side", side_oid, main_oid, "main").expect("merge");
    assert!(
        matches!(outcome, IntegrateBranchOutcome::Merged { .. }),
        "expected clean merge, got {outcome:?}"
    );

    assert_unique_stage0_paths(&repo);

    let index = repo.index_handle().load().expect("index after merge");
    assert_grit_index_matches_git_ls_files(root, &index);

    let mut side_body = fs::read_to_string(root.join("side.txt")).expect("read side.txt");
    side_body.push_str("more\n");
    fs::write(root.join("side.txt"), &side_body).expect("write side.txt");
    git(root, &["add", "side.txt"]);

    create_commit(&repo, &commit_req("next"), &mut NullProgress).expect("follow-up commit");
    assert_git_fsck_no_duplicate_entries(root);
    let ls_tree = git(root, &["ls-tree", "-r", "HEAD"]);
    let mut paths = Vec::new();
    for line in ls_tree.lines() {
        let path = line.split_whitespace().nth(3).expect("path");
        paths.push(path.to_owned());
    }
    paths.sort();
    let mut dedup = paths.clone();
    dedup.sort();
    dedup.dedup();
    assert_eq!(
        paths.len(),
        dedup.len(),
        "HEAD tree must not contain duplicate paths"
    );
}

#[test]
fn incoming_single_file_rename_does_not_duplicate_merged_index() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "user.name", "T"]);
    git(root, &["config", "user.email", "t@e.com"]);

    fs::write(root.join("name.txt"), "base\n").expect("name.txt");
    git(root, &["add", "name.txt"]);
    git(root, &["commit", "-qm", "base"]);

    git(root, &["switch", "-q", "-c", "side"]);
    fs::write(root.join("side.txt"), "side\n").expect("side.txt");
    git(root, &["add", "side.txt"]);
    git(root, &["commit", "-qm", "side"]);

    git(root, &["switch", "-q", "main"]);
    git(root, &["mv", "name.txt", "renamed.txt"]);
    git(root, &["commit", "-qm", "rename"]);

    git(root, &["switch", "-q", "side"]);
    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let main_oid = ObjectId::from_hex(git(root, &["rev-parse", "main"]).trim()).expect("main");
    let side_oid = ObjectId::from_hex(git(root, &["rev-parse", "HEAD"]).trim()).expect("side");

    let outcome =
        integrate_branch(&repo, "refs/heads/side", side_oid, main_oid, "main").expect("merge");
    assert!(
        matches!(outcome, IntegrateBranchOutcome::Merged { .. }),
        "expected clean merge, got {outcome:?}"
    );

    assert_unique_stage0_paths(&repo);
    let index = repo.index_handle().load().expect("index");
    assert_grit_index_matches_git_ls_files(root, &index);
}

#[test]
fn incoming_rename_destination_conflicts_with_independent_ours_add() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "user.name", "T"]);
    git(root, &["config", "user.email", "t@e.com"]);

    fs::write(root.join("old.txt"), "base\n").expect("old.txt");
    git(root, &["add", "old.txt"]);
    git(root, &["commit", "-qm", "base"]);

    git(root, &["switch", "-q", "-c", "side"]);
    git(root, &["rm", "old.txt"]);
    fs::write(root.join("renamed.txt"), "ours-add\n").expect("renamed.txt");
    git(root, &["add", "renamed.txt"]);
    git(root, &["commit", "-qm", "side"]);

    git(root, &["switch", "-q", "main"]);
    git(root, &["mv", "old.txt", "renamed.txt"]);
    git(root, &["commit", "-qm", "rename"]);

    git(root, &["switch", "-q", "side"]);
    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let main_oid = ObjectId::from_hex(git(root, &["rev-parse", "main"]).trim()).expect("main");
    let side_oid = ObjectId::from_hex(git(root, &["rev-parse", "HEAD"]).trim()).expect("side");

    let outcome =
        integrate_branch(&repo, "refs/heads/side", side_oid, main_oid, "main").expect("merge");
    let IntegrateBranchOutcome::Conflicts { paths, .. } = outcome else {
        panic!("expected rename/add conflict, got {outcome:?}");
    };
    assert!(
        paths.iter().any(|p| p == "renamed.txt"),
        "expected renamed.txt in conflict paths, got {paths:?}"
    );

    let index = repo.index_handle().load().expect("index");
    assert_eq!(
        git_ls_files_stage(root),
        grit_index_all_stages(&index),
        "conflicted index must match git ls-files -s"
    );
    let stages: Vec<u32> = index
        .entries()
        .iter()
        .filter(|e| e.path == b"renamed.txt")
        .map(|e| u32::from(e.stage()))
        .collect();
    assert_eq!(stages, vec![2, 3], "expected stages 2 and 3 at renamed.txt");
}
