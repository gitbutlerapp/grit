//! Regression tests for bounded `rev-list` / `grit log` fast paths.

mod support;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::error::Error;
use grit_lib::objects::odb::Odb;
use grit_lib::objects::ObjectKind;
use grit_lib::repo::{InitOptions, Repository};
use grit_lib::revwalk::rev_list::{rev_list, OrderingMode, RevListOptions};
use std::fs;
use std::path::Path;
use tempfile::tempdir;

fn empty_tree(odb: &Odb) -> grit_lib::error::Result<grit_lib::objects::ObjectId> {
    odb.write_loose_materialize(ObjectKind::Tree, b"")
}

fn write_commit(
    odb: &Odb,
    parents: &[grit_lib::objects::ObjectId],
    msg: &str,
    author_time: i64,
    committer_time: i64,
) -> grit_lib::error::Result<grit_lib::objects::ObjectId> {
    let tree = empty_tree(odb)?;
    let mut body = format!("tree {tree}\n");
    for p in parents {
        body.push_str(&format!("parent {p}\n"));
    }
    body.push_str(&format!(
        "author T <t@e.com> {author_time} +0000\ncommitter T <t@e.com> {committer_time} +0000\n\n{msg}\n"
    ));
    odb.write_loose_materialize(ObjectKind::Commit, body.as_bytes())
}

fn open_bare(dir: &Path) -> grit_lib::error::Result<Repository> {
    Repository::init(
        dir,
        &InitOptions::new()
            .with_bare(true)
            .with_initial_branch("main".to_owned())
            .with_ref_storage(grit_lib::refs::ref_storage::RefStorageFormat::Files),
    )?;
    Repository::open(dir, &RepositoryOptions::empty())
}

#[test]
fn bounded_log_on_depth_one_shallow_does_not_emit_missing_parent() {
    let dir = tempdir().expect("tempdir");
    let repo = open_bare(dir.path()).expect("repo");
    let missing: grit_lib::objects::ObjectId = "0000000000000000000000000000000000000001"
        .parse()
        .expect("oid");
    let tip = write_commit(repo.odb(), &[missing], "tip", 100, 100).expect("commit");

    fs::write(repo.git_dir().join("shallow"), format!("{tip}\n")).expect("shallow");

    let opts = RevListOptions {
        max_count: Some(11),
        ..Default::default()
    };
    let result = rev_list(&repo, &[tip.to_hex()], &[], &opts).expect("rev-list");
    assert_eq!(result.commits, vec![tip]);
}

#[test]
fn rev_list_errors_on_missing_parent_without_shallow() {
    let dir = tempdir().expect("tempdir");
    let repo = open_bare(dir.path()).expect("repo");
    let missing: grit_lib::objects::ObjectId = "0000000000000000000000000000000000000001"
        .parse()
        .expect("oid");
    let tip = write_commit(repo.odb(), &[missing], "tip", 100, 100).expect("commit");

    let opts = RevListOptions {
        max_count: Some(5),
        ..Default::default()
    };
    let err = rev_list(&repo, &[tip.to_hex()], &[], &opts).unwrap_err();
    assert!(matches!(err, Error::ObjectNotFound(_)));
}

#[test]
fn author_date_order_with_max_count_uses_author_timestamps() {
    let dir = tempdir().expect("tempdir");
    let repo = open_bare(dir.path()).expect("repo");
    let root = write_commit(repo.odb(), &[], "root", 50, 50).expect("root");
    // `a`: older author, newer committer.
    let a = write_commit(repo.odb(), &[root], "a", 100, 400).expect("a");
    // `b`: newer author, older committer.
    let b = write_commit(repo.odb(), &[root], "b", 300, 200).expect("b");

    let opts = RevListOptions {
        max_count: Some(2),
        ordering: OrderingMode::AuthorDateWalk,
        ..Default::default()
    };
    let result = rev_list(&repo, &[a.to_hex(), b.to_hex()], &[], &opts).expect("rev-list");
    assert_eq!(result.commits, vec![b, a]);
}

#[test]
fn exclude_first_parent_only_with_max_count_matches_git() {
    let dir = tempdir().expect("tempdir");
    let repo = open_bare(dir.path()).expect("repo");
    let root = write_commit(repo.odb(), &[], "root", 10, 10).expect("root");
    let first = write_commit(repo.odb(), &[root], "first", 20, 20).expect("first");
    let second = write_commit(repo.odb(), &[root], "second", 30, 30).expect("second");
    let merge = write_commit(repo.odb(), &[first, second], "merge", 40, 40).expect("merge");

    let opts = RevListOptions {
        max_count: Some(1),
        exclude_first_parent_only: true,
        ..Default::default()
    };
    let result = rev_list(&repo, &[second.to_hex()], &[merge.to_hex()], &opts).expect("rev-list");
    assert_eq!(result.commits, vec![second]);
}

#[test]
fn bounded_path_limit_stops_at_first_matching_commit() {
    let repo = support::HistoryRepo::init();
    let wt = repo.worktree();
    let touch = support::commit_file(wt, "tracked.txt", "t\n", "touch tracked");
    for i in 0..120 {
        let content = format!("n{i}\n");
        let msg = format!("noise {i}");
        support::commit_file(wt, "noise.txt", &content, &msg);
    }

    let grit_repo = Repository::discover(Some(wt), &RepositoryOptions::empty()).expect("discover");
    let opts = RevListOptions {
        max_count: Some(1),
        paths: vec!["tracked.txt".to_owned()],
        ..Default::default()
    };
    let result = rev_list(&grit_repo, &[String::from("HEAD")], &[], &opts).expect("rev-list");
    assert_eq!(result.commits, vec![touch]);

    support::assert_grit_commits_match_git(
        wt,
        &["-n", "1", "HEAD", "--", "tracked.txt"],
        &[String::from("HEAD")],
        &[],
        &opts,
    );
}

#[test]
fn bounded_path_limit_skip_and_max_match_git() {
    let repo = support::HistoryRepo::init();
    let wt = repo.worktree();
    support::commit_file(wt, "tracked.txt", "v0\n", "v0");
    let mut prior = support::commit_file(wt, "tracked.txt", "v1\n", "v1");
    for i in 2..80 {
        let content = format!("v{i}\n");
        let msg = format!("v{i}");
        prior = support::commit_file(wt, "tracked.txt", &content, &msg);
    }
    let _newest = prior;

    let opts = RevListOptions {
        max_count: Some(3),
        skip: 1,
        paths: vec!["tracked.txt".to_owned()],
        ..Default::default()
    };
    support::assert_grit_commits_match_git(
        wt,
        &["-n", "3", "--skip", "1", "HEAD", "--", "tracked.txt"],
        &[String::from("HEAD")],
        &[],
        &opts,
    );
}

#[test]
fn bounded_path_limit_zero_emits_nothing_at_matching_tip() {
    let repo = support::HistoryRepo::init();
    let wt = repo.worktree();
    support::commit_file(wt, "tracked.txt", "v\n", "touch");

    let opts = RevListOptions {
        max_count: Some(0),
        paths: vec!["tracked.txt".to_owned()],
        ..Default::default()
    };
    support::assert_grit_commits_match_git(
        wt,
        &["--max-count=0", "HEAD", "--", "tracked.txt"],
        &[String::from("HEAD")],
        &[],
        &opts,
    );
}

#[test]
fn large_max_count_without_paths_matches_git() {
    let repo = support::HistoryRepo::init();
    let wt = repo.worktree();
    for i in 0..120 {
        let content = format!("n{i}\n");
        let msg = format!("noise {i}");
        support::commit_file(wt, "noise.txt", &content, &msg);
    }

    let grit_repo = Repository::discover(Some(wt), &RepositoryOptions::empty()).expect("discover");
    let opts = RevListOptions {
        max_count: Some(10_000),
        ..Default::default()
    };
    support::assert_grit_commits_match_git(
        wt,
        &["-n", "10000", "HEAD"],
        &[String::from("HEAD")],
        &[],
        &opts,
    );
    let result = rev_list(&grit_repo, &[String::from("HEAD")], &[], &opts).expect("rev-list");
    assert!(!result.commits.is_empty());
}

#[test]
fn bounded_path_limit_extreme_max_count_does_not_panic() {
    let repo = support::HistoryRepo::init();
    let wt = repo.worktree();
    support::commit_file(wt, "tracked.txt", "v\n", "touch");

    let grit_repo = Repository::discover(Some(wt), &RepositoryOptions::empty()).expect("discover");
    let opts = RevListOptions {
        max_count: Some(usize::MAX),
        paths: vec!["tracked.txt".to_owned()],
        ..Default::default()
    };
    let result = rev_list(&grit_repo, &[String::from("HEAD")], &[], &opts).expect("rev-list");
    assert_eq!(result.commits.len(), 1);
}
