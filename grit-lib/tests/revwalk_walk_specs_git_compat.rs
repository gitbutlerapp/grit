//! `RevWalk::walk` revision specs vs system `git rev-list` (issue #1879).
//!
//! `walk` takes exactly one revision or range. Symmetric ranges (`A...B`) must list the
//! symmetric difference like `git rev-list A...B` and [`RevWalk::commits`]; inputs that only
//! the token-based API understands (options, `--`, several tokens) fail with a typed error.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use grit_lib::config::environment::{Environment, RepositoryOptions};
use grit_lib::error::Error;
use grit_lib::repo::Repository;
use grit_lib::revwalk::rev_list::{rev_list, RevListOptions};
use grit_lib::revwalk::rev_list_error::RevListError;
use grit_lib::revwalk::RevWalkListOptions;
use grit_test_support::{git, unique_tmp, write_file};

/// The issue's scratch repository: three base commits, `topic` with two commits, `main` with
/// three more on top of the shared base.
fn diverged_repo() -> PathBuf {
    let wt = unique_tmp("revwalk-walk-specs", "diverged");
    git(&wt, &["init", "-q", "-b", "main", "."]);
    let commit = |file: &str, body: &str, msg: &str| {
        write_file(&wt, file, body);
        git(&wt, &["add", file]);
        git(&wt, &["commit", "-qm", msg]);
    };
    for i in 1..=3 {
        commit("f", &format!("{i}\n"), &format!("base{i}"));
    }
    git(&wt, &["switch", "-qc", "topic"]);
    for i in 1..=2 {
        commit("t", &format!("t{i}\n"), &format!("topic{i}"));
    }
    git(&wt, &["switch", "-q", "main"]);
    for i in 1..=3 {
        commit("m", &format!("m{i}\n"), &format!("main{i}"));
    }
    wt
}

fn open(wt: &Path) -> Repository {
    let mut env = Environment::empty();
    env.cwd = wt.to_path_buf();
    env.pwd = Some(wt.to_string_lossy().into_owned());
    Repository::discover(Some(wt), &RepositoryOptions::with_environment(env)).expect("discover")
}

fn git_rev_list(wt: &Path, spec: &str) -> Vec<String> {
    git(wt, &["rev-list", spec])
        .lines()
        .map(str::to_owned)
        .collect()
}

fn walk_hex(repo: &Repository, spec: &str) -> Vec<String> {
    repo.rev_walk()
        .walk(spec, &RevListOptions::default())
        .unwrap_or_else(|e| panic!("walk({spec}): {e}"))
        .commits
        .iter()
        .map(|oid| oid.to_hex())
        .collect()
}

fn commits_hex(repo: &Repository, spec: &str) -> Vec<String> {
    repo.rev_walk()
        .commits(&[spec], &RevWalkListOptions::default())
        .unwrap_or_else(|e| panic!("commits([{spec}]): {e}"))
        .iter()
        .map(|oid| oid.to_hex())
        .collect()
}

fn as_set(oids: &[String]) -> BTreeSet<&str> {
    oids.iter().map(String::as_str).collect()
}

#[test]
fn walk_symmetric_and_two_dot_ranges_match_git_rev_list() {
    let wt = diverged_repo();
    let repo = open(&wt);
    for (spec, expected_len) in [
        ("main...topic", 5),
        ("topic...main", 5),
        ("main...", 0),
        ("...main", 0),
        ("topic...", 5),
        ("...topic", 5),
        ("main..topic", 2),
        ("topic..main", 3),
        ("main", 6),
        ("^topic", 0),
    ] {
        let expected = git_rev_list(&wt, spec);
        let walked = walk_hex(&repo, spec);
        assert_eq!(walked.len(), expected_len, "walk({spec}) count");
        assert_eq!(walked.len(), expected.len(), "walk({spec}) vs git count");
        assert_eq!(
            as_set(&walked),
            as_set(&expected),
            "walk({spec}) vs git set"
        );
        assert_eq!(walked, commits_hex(&repo, spec), "walk({spec}) vs commits");
    }
}

#[test]
fn walk_symmetric_range_from_detached_side_matches_git() {
    let wt = diverged_repo();
    git(&wt, &["switch", "-q", "topic"]);
    let repo = open(&wt);
    for spec in ["main...", "...main", "main...HEAD", "HEAD...main"] {
        let expected = git_rev_list(&wt, spec);
        assert_eq!(expected.len(), 5, "git rev-list {spec}");
        let walked = walk_hex(&repo, spec);
        assert_eq!(
            as_set(&walked),
            as_set(&expected),
            "walk({spec}) vs git set"
        );
        assert_eq!(walked, commits_hex(&repo, spec), "walk({spec}) vs commits");
    }
}

#[test]
fn walk_symmetric_range_honours_max_count_and_left_right() {
    let wt = diverged_repo();
    let repo = open(&wt);
    let limited = repo
        .rev_walk()
        .walk(
            "main...topic",
            &RevListOptions {
                max_count: Some(2),
                ..RevListOptions::default()
            },
        )
        .expect("walk -n 2");
    let expected: Vec<String> = git(&wt, &["rev-list", "-n", "2", "main...topic"])
        .lines()
        .map(str::to_owned)
        .collect();
    let got: Vec<String> = limited.commits.iter().map(|o| o.to_hex()).collect();
    assert_eq!(got, expected);

    let left_right = repo
        .rev_walk()
        .walk(
            "main...topic",
            &RevListOptions {
                left_right: true,
                ..RevListOptions::default()
            },
        )
        .expect("walk --left-right");
    let git_marks: BTreeSet<String> = git(&wt, &["rev-list", "--left-right", "main...topic"])
        .lines()
        .map(str::to_owned)
        .collect();
    let grit_marks: BTreeSet<String> = left_right
        .commits
        .iter()
        .map(|oid| {
            let mark = if left_right.left_right_map.get(oid).copied().unwrap_or(false) {
                '<'
            } else {
                '>'
            };
            format!("{mark}{}", oid.to_hex())
        })
        .collect();
    assert_eq!(grit_marks, git_marks);
}

#[test]
fn walk_rejects_specs_that_need_the_token_api() {
    let wt = diverged_repo();
    let repo = open(&wt);
    for spec in [
        "--all",
        "--branches",
        "--",
        "-n",
        "HEAD ^HEAD~2",
        "main\ttopic",
    ] {
        let err = repo
            .rev_walk()
            .walk(spec, &RevListOptions::default())
            .expect_err(spec);
        match err {
            Error::RevList(RevListError::UnsupportedWalkSpec { spec: got }) => {
                assert_eq!(got, spec, "error keeps the full spec");
            }
            other => panic!("walk({spec:?}): expected UnsupportedWalkSpec, got {other:?}"),
        }
    }
}

#[test]
fn walk_accepts_single_specs_containing_spaces() {
    let wt = diverged_repo();
    let repo = open(&wt);
    let search = walk_hex(&repo, ":/topic2");
    assert_eq!(search, git_rev_list(&wt, ":/topic2"));
    let spaced = walk_hex(&repo, "main^{/main 1|main1}");
    assert_eq!(spaced, git_rev_list(&wt, "main^{/main 1|main1}"));
}

#[test]
fn rev_list_rejects_unexpanded_symmetric_range() {
    let wt = diverged_repo();
    let repo = open(&wt);
    let err = rev_list(
        &repo,
        &["main...topic".to_owned()],
        &[],
        &RevListOptions::default(),
    )
    .expect_err("raw A...B must not resolve to its merge base");
    assert!(
        matches!(
            &err,
            Error::RevList(RevListError::UnexpandedSymmetricRange { spec }) if spec == "main...topic"
        ),
        "{err:?}"
    );
}
