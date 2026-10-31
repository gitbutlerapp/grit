//! Issue #2359: file/directory merge keeps the directory and parks the file at `path~label`.

use std::path::Path;

use crate::support::{git, git_ls_files_stage, open_grit_repo};
use grit_lib::error::Error;
use grit_lib::objects::ObjectId;
use grit_lib::porcelain::commit::{commit_with_scope, CommitRequest, CommitScope};
use grit_lib::porcelain::merge::{abort_branch_merge, integrate_branch, IntegrateBranchOutcome};

fn build_issue_2359_fixture(root: &Path) -> (ObjectId, ObjectId) {
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "user.name", "N"]);
    git(root, &["config", "user.email", "n@x"]);
    std::fs::write(root.join("f"), "1\n").expect("write f");
    git(root, &["add", "f"]);
    git(root, &["commit", "-qm", "base"]);
    git(root, &["switch", "-c", "side"]);
    std::fs::write(root.join("f"), "2\n").expect("write f side");
    git(root, &["add", "f"]);
    git(root, &["commit", "-qm", "side"]);
    let side: ObjectId = git(root, &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .expect("side");
    git(root, &["switch", "main"]);
    std::fs::remove_file(root.join("f")).expect("remove f");
    std::fs::create_dir(root.join("f")).expect("mkdir f");
    std::fs::write(root.join("f/i"), "i\n").expect("write f/i");
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "main"]);
    let main: ObjectId = git(root, &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .expect("main");
    (main, side)
}

#[test]
fn integrate_branch_df_conflict_parks_file_and_keeps_directory() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (main, side) = build_issue_2359_fixture(tmp.path());
    let repo = open_grit_repo(tmp.path());

    let outcome = integrate_branch(
        &repo,
        "refs/heads/main",
        main,
        side,
        "side",
        &mut grit_lib::progress::NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("merge");
    let IntegrateBranchOutcome::Conflicts { paths, .. } = outcome else {
        panic!("expected merge conflicts");
    };
    assert!(paths.iter().any(|p| p == "f~side"));

    assert!(tmp.path().join("f/i").is_file());
    assert!(tmp.path().join("f~side").is_file());
    assert!(!tmp.path().join("f").is_file());

    let rows = git_ls_files_stage(tmp.path());
    let parked: Vec<_> = rows
        .iter()
        .filter(|(path, _, _, _)| path == "f~side")
        .collect();
    assert_eq!(parked.len(), 2, "f~side should have stages 1 and 3");
    assert!(rows
        .iter()
        .any(|(path, stage, _, _)| path == "f/i" && *stage == 0));

    let err = commit_with_scope(
        &repo,
        &CommitRequest::new("resolved"),
        CommitScope::Staged,
        &mut grit_lib::progress::NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect_err("staged commit with unmerged paths");
    assert!(matches!(err, Error::IndexUnmerged));

    abort_branch_merge(&repo).expect("abort");
    assert!(tmp.path().join("f/i").is_file());
    assert!(!tmp.path().join("f~side").exists());
    assert_eq!(
        git(tmp.path(), &["status", "--short"]).trim(),
        "",
        "work tree clean after abort"
    );
}
