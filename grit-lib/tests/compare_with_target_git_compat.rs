//! [`compare_with_target`] ahead counts and oid lists vs `git rev-list`.

use std::path::Path;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::error::Error;
use grit_lib::objects::ObjectId;
use grit_lib::refs::branch_tracking::{
    compare_with_target, compare_with_target_optional, BaseSource, TargetComparisonOptions,
    TargetSpec,
};
use grit_lib::repo::Repository;
use grit_test_support::{git, unique_tmp, write_file};

fn open(dir: &Path) -> Repository {
    Repository::discover(Some(dir), &RepositoryOptions::empty()).expect("open repo")
}

fn rev(dir: &Path, spec: &str) -> ObjectId {
    git(dir, &["rev-parse", spec])
        .trim()
        .parse()
        .expect("rev-parse prints an oid")
}

fn commit(dir: &Path, file: &str, message: &str) {
    write_file(dir, file, message);
    git(dir, &["add", file]);
    git(
        dir,
        &[
            "-c",
            "user.name=T",
            "-c",
            "user.email=t@example.com",
            "commit",
            "-q",
            "-m",
            message,
        ],
    );
}

fn git_ahead_count(dir: &Path, target: &str) -> usize {
    let range = format!("{target}..HEAD");
    git(dir, &["rev-list", "--count", &range])
        .trim()
        .parse()
        .expect("count")
}

fn git_ahead_list(dir: &Path, target: &str, limit: usize) -> Vec<ObjectId> {
    let range = format!("{target}..HEAD");
    git(dir, &["rev-list", "-n", &limit.to_string(), &range])
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| l.parse().expect("oid"))
        .collect()
}

fn assert_matches_git(dir: &Path, target: &str, limit: usize) {
    let repo = open(dir);
    let opts = TargetComparisonOptions {
        target: Some(TargetSpec(target.to_owned())),
        list_limit: limit,
        include_behind: false,
    };
    let cmp = compare_with_target(&repo, &opts).expect("compare");
    assert_eq!(
        compare_with_target_optional(&repo, &opts).expect("optional compare"),
        Some(cmp.clone())
    );
    assert_eq!(cmp.ahead, git_ahead_count(dir, target));
    assert_eq!(cmp.newest_ahead, git_ahead_list(dir, target, limit));
    assert_eq!(cmp.base.source, BaseSource::Explicit);
    assert_eq!(cmp.base.name, target);
}

fn topic_ahead_fixture(tag: &str) -> std::path::PathBuf {
    let root = unique_tmp("compare-target", tag);
    git(&root, &["init", "-q", "-b", "main"]);
    commit(&root, "a", "m1");
    commit(&root, "a", "m2");
    commit(&root, "a", "m3");
    git(&root, &["branch", "-q", "topic"]);
    git(&root, &["checkout", "-q", "topic"]);
    commit(&root, "b", "t1");
    commit(&root, "b", "t2");
    root
}

#[test]
fn compare_with_target_local_branch_ref() {
    let dir = topic_ahead_fixture("local-branch");
    assert_matches_git(&dir, "main", 100);
}

#[test]
fn compare_with_target_remote_tracking_ref() {
    let root = unique_tmp("compare-target", "remote");
    let origin = root.join("origin");
    std::fs::create_dir_all(&origin).expect("mkdir");
    git(&origin, &["init", "-q", "-b", "main"]);
    commit(&origin, "a", "o1");
    commit(&origin, "a", "o2");
    git(&root, &["clone", "-q", "origin", "clone"]);
    let clone = root.join("clone");
    commit(&clone, "b", "local");
    git(&clone, &["fetch", "-q"]);
    assert_matches_git(&clone, "origin/main", 100);
}

#[test]
fn compare_with_target_tag() {
    let dir = topic_ahead_fixture("tag");
    git(&dir, &["tag", "v1", "main"]);
    assert_matches_git(&dir, "v1", 100);
}

#[test]
fn compare_with_target_raw_oid() {
    let dir = topic_ahead_fixture("oid");
    let main_oid = rev(&dir, "main");
    assert_matches_git(&dir, &main_oid.to_hex(), 100);
}

#[test]
fn compare_with_target_unknown_errors() {
    let dir = topic_ahead_fixture("unknown");
    let repo = open(&dir);
    let opts = TargetComparisonOptions {
        target: Some(TargetSpec("no-such-ref".into())),
        list_limit: 100,
        include_behind: false,
    };
    let err = compare_with_target(&repo, &opts).unwrap_err();
    assert!(
        matches!(err, Error::ObjectNotFound(_))
            || matches!(err, Error::InvalidRef(_))
            || matches!(err, Error::RevParse(_))
    );
}

#[test]
fn compare_with_target_diverged_history() {
    let root = unique_tmp("compare-target", "diverged");
    git(&root, &["init", "-q", "-b", "main"]);
    commit(&root, "a", "base");
    git(&root, &["branch", "-q", "topic"]);
    git(&root, &["checkout", "-q", "topic"]);
    commit(&root, "b", "topic-only");
    git(&root, &["checkout", "-q", "main"]);
    commit(&root, "a", "main-only");
    git(&root, &["checkout", "-q", "topic"]);
    assert_matches_git(&root, "main", 100);
}

#[test]
fn compare_with_target_optional_none_without_candidates() {
    let root = unique_tmp("compare-target", "no-target");
    git(&root, &["init", "-q", "-b", "trunk"]);
    commit(&root, "f", "only");
    let repo = open(&root);
    let opts = TargetComparisonOptions {
        target: None,
        list_limit: 100,
        include_behind: false,
    };
    assert!(compare_with_target_optional(&repo, &opts)
        .expect("optional")
        .is_none());
}

#[test]
fn both_target_comparisons_keep_exact_counts_with_bounded_lists() {
    let dir = topic_ahead_fixture("bounded-lists");
    git(&dir, &["checkout", "-q", "main"]);
    commit(&dir, "a", "main-only");
    git(&dir, &["checkout", "-q", "topic"]);
    let repo = open(&dir);
    for target in [None, Some(TargetSpec("main".into()))] {
        for list_limit in [0, 1, 100] {
            for include_behind in [false, true] {
                let opts = TargetComparisonOptions {
                    target: target.clone(),
                    list_limit,
                    include_behind,
                };
                let cmp = compare_with_target(&repo, &opts).expect("compare");
                assert_eq!(
                    compare_with_target_optional(&repo, &opts).expect("optional compare"),
                    Some(cmp.clone())
                );
                let counts = git(
                    &dir,
                    &["rev-list", "--left-right", "--count", "HEAD...main"],
                );
                let counts: Vec<usize> = counts
                    .split_whitespace()
                    .map(|n| n.parse().unwrap())
                    .collect();
                assert_eq!((cmp.ahead, cmp.behind), (counts[0], counts[1]));
                assert_eq!(cmp.newest_ahead, git_ahead_list(&dir, "main", list_limit));
                let behind_limit = if include_behind { list_limit } else { 0 };
                let behind = git(
                    &dir,
                    &["rev-list", "-n", &behind_limit.to_string(), "HEAD..main"],
                );
                let behind: Vec<ObjectId> =
                    behind.lines().map(|oid| oid.parse().unwrap()).collect();
                assert_eq!(cmp.newest_behind, behind);
            }
        }
    }
}
