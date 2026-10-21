//! Issue #1005: modify/delete merge conflicts must be resolvable via grit add/rm.

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::ObjectId;
use grit_lib::porcelain::add::{stage, StageOptions};
use grit_lib::porcelain::merge::{integrate_branch, IntegrateBranchOutcome, MergeConflictKind};
use grit_lib::porcelain::paths::{remove_paths, RemoveOptions};
use grit_lib::progress::NullProgress;
use grit_lib::repo::Repository;
use grit_test_support::git;
use std::fs;

fn issue_1005_repo() -> (tempfile::TempDir, Repository) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-b", "main"]);
    git(root, &["config", "user.name", "T"]);
    git(root, &["config", "user.email", "t@e.com"]);

    fs::write(root.join("f"), "base\n").expect("f");
    fs::write(root.join("g"), "keep\n").expect("g");
    git(root, &["add", "."]);
    git(root, &["commit", "-m", "base"]);

    git(root, &["switch", "-c", "feat"]);
    fs::write(root.join("f"), "base\nfeat\n").expect("feat f");
    git(root, &["add", "f"]);
    git(root, &["commit", "-m", "feat"]);

    git(root, &["switch", "main"]);
    git(root, &["rm", "f"]);
    git(root, &["commit", "-m", "delete f"]);

    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    (tmp, repo)
}

#[test]
fn modify_delete_conflict_materializes_incoming_version_and_resolves_with_add_or_rm() {
    let (_tmp, repo) = issue_1005_repo();
    let root = repo.work_tree().expect("worktree");

    let main_oid = ObjectId::from_hex(&git(root, &["rev-parse", "HEAD"]).trim()).expect("main");
    let feat_oid = ObjectId::from_hex(&git(root, &["rev-parse", "feat"]).trim()).expect("feat");

    let outcome =
        integrate_branch(&repo, "refs/heads/main", main_oid, feat_oid, "feat").expect("merge");
    let IntegrateBranchOutcome::Conflicts { paths, details } = outcome else {
        panic!("expected conflicts");
    };
    assert_eq!(paths, vec!["f".to_owned()]);
    assert_eq!(details.len(), 1);
    assert_eq!(details[0].path, "f");
    assert!(matches!(
        details[0].kind,
        MergeConflictKind::ModifyDelete {
            deleted_in_current: true
        }
    ));

    assert!(
        root.join("f").is_file(),
        "incoming branch version should appear in the work tree"
    );
    assert_eq!(
        fs::read_to_string(root.join("f")).expect("read f"),
        "base\nfeat\n"
    );

    let index = repo.index_handle().load().expect("index");
    assert!(index.has_unmerged_entries());

    // grit add f — keep the file
    let add_keep = stage(
        &repo,
        &StageOptions {
            pathspecs: vec!["f".into()],
            ..StageOptions::default()
        },
        &mut NullProgress,
    )
    .expect("add keep");
    assert_eq!(add_keep.modified.len(), 1);
    let index = repo.index_handle().load().expect("index after add");
    assert!(!index.has_unmerged_entries());
    assert!(index.get(b"f", 0).is_some());
}

#[test]
fn modify_delete_conflict_resolves_by_accepting_deletion() {
    let (_tmp, repo) = issue_1005_repo();
    let root = repo.work_tree().expect("worktree");
    let main_oid = ObjectId::from_hex(&git(root, &["rev-parse", "HEAD"]).trim()).expect("main");
    let feat_oid = ObjectId::from_hex(&git(root, &["rev-parse", "feat"]).trim()).expect("feat");
    let outcome =
        integrate_branch(&repo, "refs/heads/main", main_oid, feat_oid, "feat").expect("merge");
    assert!(matches!(outcome, IntegrateBranchOutcome::Conflicts { .. }));

    let rm_out = remove_paths(
        &repo,
        &RemoveOptions {
            pathspecs: vec!["f".into()],
            ..RemoveOptions::default()
        },
    )
    .expect("rm f");
    assert_eq!(rm_out.removed, vec!["f".to_owned()]);
    let index = repo.index_handle().load().expect("index after rm");
    assert!(!index.has_unmerged_entries());
    assert!(index.get(b"f", 0).is_none());
    assert!(!root.join("f").exists());
}

#[test]
fn modify_delete_merge_does_not_mark_unrelated_files_modified() {
    let (_tmp, repo) = issue_1005_repo();
    let root = repo.work_tree().expect("worktree");
    let main_oid = ObjectId::from_hex(&git(root, &["rev-parse", "HEAD"]).trim()).expect("main");
    let feat_oid = ObjectId::from_hex(&git(root, &["rev-parse", "feat"]).trim()).expect("feat");
    integrate_branch(&repo, "refs/heads/main", main_oid, feat_oid, "feat").expect("merge");

    use grit_lib::porcelain::status::{status, StatusOptions};
    let model = status(
        &repo,
        &StatusOptions::default(),
        &mut grit_lib::progress::NullProgress,
    )
    .expect("status");
    assert!(
        model.unstaged.iter().all(|e| e.path() != "g"),
        "g should not be unstaged-modified after conflict merge: {:?}",
        model.unstaged
    );
}
