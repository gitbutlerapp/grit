//! Rev-parse diagnose helpers: tree/index prefix mismatches and reflog edges.

mod support;

use std::path::Path;

use grit_lib::config::environment::{Environment, RepositoryOptions};
use grit_lib::error::Error;
use grit_lib::repo::Repository;
use grit_lib::revwalk::rev_parse::{
    resolve_at_minus_to_oid, resolve_revision_for_verify, resolve_treeish_blob_at_path,
};
use grit_lib::revwalk::rev_parse_error::RevParseError;

use support::{commit_file, git, HistoryRepo};

fn open_grit_at(wt: &Path, cwd: &Path) -> Repository {
    let mut env = Environment::empty();
    env.cwd = cwd.to_path_buf();
    env.pwd = Some(cwd.to_string_lossy().into_owned());
    let options = RepositoryOptions::with_environment(env);
    Repository::discover(Some(wt), &options).expect("discover")
}

fn expect_rev_parse(repo: &Repository, spec: &str, pred: fn(&RevParseError) -> bool) {
    let err = resolve_revision_for_verify(repo, spec).expect_err(spec);
    let Error::RevParse(inner) = err else {
        panic!("{spec}: expected RevParse, got {err}");
    };
    assert!(pred(&inner), "{spec}: got {inner:?}");
}

#[test]
fn tree_path_prefix_exists_from_subdirectory() {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    commit_file(wt, "nested/leaf.txt", "leaf\n", "nested");
    let sub = wt.join("nested");
    let repo = open_grit_at(wt, &sub);
    expect_rev_parse(&repo, "HEAD:leaf.txt", |e| {
        matches!(e, RevParseError::PathPrefixExists { .. })
    });
    let _ = resolve_treeish_blob_at_path(&repo, "HEAD:nested/leaf.txt");
}

#[test]
fn index_prefix_mismatch_from_subdirectory() {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    commit_file(wt, "nested/leaf.txt", "base\n", "base");
    let sub = wt.join("nested");
    let repo = open_grit_at(wt, &sub);
    expect_rev_parse(&repo, ":2:leaf.txt", |e| {
        matches!(
            e,
            RevParseError::PathIndexPrefixMismatch { .. }
                | RevParseError::PathWrongIndexStage { .. }
                | RevParseError::PathMissingFromWorktreeAndIndex { .. }
        )
    });
}

#[test]
fn reflog_zero_without_reflog_file() {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    commit_file(wt, "f.txt", "1\n", "one");
    git(wt, &["update-ref", "refs/heads/novel", "HEAD"]);
    let log = wt.join(".git/logs/refs/heads/novel");
    if log.exists() {
        std::fs::remove_file(&log).expect("remove novel reflog");
    }
    let repo = open_grit_at(wt, wt);
    expect_rev_parse(&repo, "novel@{0}", |e| {
        matches!(e, RevParseError::ReflogEmpty { .. })
    });
}

#[test]
fn at_minus_resolves_previous_branch_oid() {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    commit_file(wt, "f.txt", "1\n", "one");
    git(wt, &["checkout", "-b", "first"]);
    commit_file(wt, "f.txt", "2\n", "two");
    git(wt, &["checkout", "-b", "second"]);
    commit_file(wt, "f.txt", "3\n", "three");
    git(wt, &["checkout", "main"]);
    let repo = open_grit_at(wt, wt);
    let oid = resolve_at_minus_to_oid(&repo, "@{-1}")
        .expect("resolve")
        .expect("some");
    let git_oid = support::git_rev_parse_verify(wt, "@{-1}").expect("git @{-1}");
    assert_eq!(oid, git_oid);
}
