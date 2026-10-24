//! Issue #1670: `grit add .` during a merge must stage resolved paths and name marked ones.

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::error::Error;
use grit_lib::objects::ObjectId;
use grit_lib::porcelain::add::{stage, StageOptions};
use grit_lib::porcelain::merge::{integrate_branch, IntegrateBranchOutcome};
use grit_lib::progress::NullProgress;
use grit_lib::repo::Repository;
use grit_test_support::git;
use std::fs;

fn issue_1670_repo() -> (tempfile::TempDir, Repository) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "user.name", "t"]);
    git(root, &["config", "user.email", "t@e.test"]);

    fs::write(root.join("a.txt"), "1\n2\n3\n").expect("a");
    fs::copy(root.join("a.txt"), root.join("b.txt")).expect("b");
    fs::write(root.join("c.txt"), "new\n").expect("c");
    git(root, &["add", "."]);
    git(root, &["commit", "-qm", "base"]);

    git(root, &["switch", "-qc", "topic"]);
    fs::write(root.join("a.txt"), "1\n2\n3\ntopic\n").expect("a topic");
    fs::write(root.join("b.txt"), "1\n2\n3\ntopic\n").expect("b topic");
    git(root, &["commit", "-qam", "topic"]);

    git(root, &["switch", "-q", "main"]);
    fs::write(root.join("a.txt"), "1\n2\n3\nmain\n").expect("a main");
    fs::write(root.join("b.txt"), "1\n2\n3\nmain\n").expect("b main");
    git(root, &["commit", "-qam", "main"]);

    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    (tmp, repo)
}

#[test]
fn add_dot_stages_resolved_and_unrelated_while_naming_marked_conflicts() {
    let (_tmp, repo) = issue_1670_repo();
    let root = repo.work_tree().expect("worktree");

    let main_oid = ObjectId::from_hex(git(root, &["rev-parse", "HEAD"]).trim()).expect("main");
    let topic_oid = ObjectId::from_hex(git(root, &["rev-parse", "topic"]).trim()).expect("topic");
    let outcome =
        integrate_branch(&repo, "refs/heads/main", main_oid, topic_oid, "topic").expect("merge");
    assert!(matches!(outcome, IntegrateBranchOutcome::Conflicts { .. }));

    fs::write(root.join("a.txt"), "1\n2\n3\nok\n").expect("resolve a");
    fs::write(root.join("c.txt"), "unrelated\n").expect("edit c");

    let err = stage(
        &repo,
        &StageOptions {
            pathspecs: vec![".".into()],
            ..StageOptions::default()
        },
        &mut NullProgress,
    )
    .unwrap_err();
    let Error::UnmergedConflictMarkers { paths } = err else {
        panic!("expected marker paths, got {err}");
    };
    assert_eq!(paths, vec!["b.txt".to_owned()]);

    let index = repo.index_handle().load().expect("index");
    assert!(index.get(b"a.txt", 0).is_some());
    assert!(index.get(b"c.txt", 0).is_some());
    assert!(index.get(b"b.txt", 0).is_none());
    assert!(index.has_unmerged_entries());
}

#[test]
fn add_single_marked_unmerged_path_names_that_path() {
    let (_tmp, repo) = issue_1670_repo();
    let root = repo.work_tree().expect("worktree");

    let main_oid = ObjectId::from_hex(git(root, &["rev-parse", "HEAD"]).trim()).expect("main");
    let topic_oid = ObjectId::from_hex(git(root, &["rev-parse", "topic"]).trim()).expect("topic");
    integrate_branch(&repo, "refs/heads/main", main_oid, topic_oid, "topic").expect("merge");

    let err = stage(
        &repo,
        &StageOptions {
            pathspecs: vec!["b.txt".into()],
            ..StageOptions::default()
        },
        &mut NullProgress,
    )
    .unwrap_err();
    let Error::UnmergedConflictMarkers { paths } = err else {
        panic!("expected marker paths, got {err}");
    };
    assert_eq!(paths, vec!["b.txt".to_owned()]);
}
