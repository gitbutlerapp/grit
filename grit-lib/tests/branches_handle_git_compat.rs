//! `repo.branches()` round-trips with the system `git`: create, list, delete, and rename
//! through the handle leave a repository `git fsck --strict` accepts.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use grit_lib::porcelain::branch::{
    BranchCreateOptions, BranchDeleteMode, BranchDeleteOptions, BranchRenameOptions,
};
use grit_lib::refs::branch_tracking::{BranchDetail, BranchListQuery, BranchScope};
use grit_lib::refs::listing::RefQuery;
use grit_lib::repo::Repository;
use grit_test_support::{git, git_cmd};

fn init() -> (tempfile::TempDir, Repository) {
    let dir = tempfile::tempdir().unwrap();
    git_cmd(&["init", "-q", "-b", "main"])
        .in_dir(dir.path())
        .exec();
    git(dir.path(), &["config", "user.email", "h@grit.test"]);
    git(dir.path(), &["config", "user.name", "Handle"]);
    std::fs::write(dir.path().join("README.md"), "base\n").unwrap();
    git(dir.path(), &["add", "README.md"]);
    git(dir.path(), &["commit", "-qm", "base"]);
    let repo = Repository::discover(
        Some(dir.path()),
        &grit_lib::config::environment::RepositoryOptions::empty(),
    )
    .unwrap();
    (dir, repo)
}

#[test]
fn branches_handle_round_trips_with_git() {
    let (dir, repo) = init();
    let tip = repo
        .references()
        .resolve(grit_lib::refs::store::RefNameRef::new("HEAD").expect("valid ref name"))
        .unwrap();

    repo.branches()
        .create(
            &BranchCreateOptions::at_head("feature")
                .with_start(Some(grit_lib::porcelain::branch::StartPoint::new(
                    tip, "HEAD",
                )))
                .with_track(grit_lib::porcelain::branch::TrackMode::Never),
        )
        .expect("create");

    let listed = repo
        .branches()
        .list(
            &BranchListQuery::default()
                .with_scope(BranchScope::Local)
                .with_refs(RefQuery::default()),
            BranchDetail::Names,
        )
        .expect("list");
    assert!(listed.iter().any(|b| b.name == "feature"));

    repo.branches()
        .rename(&BranchRenameOptions::new("feature", "renamed"))
        .expect("rename");

    repo.branches()
        .delete(
            "renamed",
            &BranchDeleteOptions::default().with_mode(BranchDeleteMode::Force),
        )
        .expect("delete");

    let _ = git(dir.path(), &["fsck", "--strict"]);
    let names: Vec<_> = git(dir.path(), &["branch", "--format=%(refname:short)"])
        .lines()
        .map(str::to_owned)
        .collect();
    assert!(!names.iter().any(|n| n == "renamed" || n == "feature"));
}
