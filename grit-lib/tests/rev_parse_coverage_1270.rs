//! Targeted rev-parse coverage for index DWIM, diagnose helpers, and conflict stages.

use crate::support;
use grit_lib::error::RevisionError;

use std::fs;
use std::path::Path;

use grit_lib::config::environment::{Environment, RepositoryOptions};
use grit_lib::error::Error;
use grit_lib::repo::Repository;
use grit_lib::revwalk::rev_parse::{abbreviate_ref_name, upstream_suffix_info};
use grit_lib::revwalk::rev_parse_error::RevParseError;

use support::{
    commit_file, git, git_merge_expect_conflict, git_rev_parse_verify, oracle_one, HistoryRepo,
    ResolveMode,
};

fn open_grit_at(wt: &Path, cwd: &Path) -> Repository {
    let mut env = Environment::empty();
    env.cwd = cwd.to_path_buf();
    env.pwd = Some(cwd.to_string_lossy().into_owned());
    let options = RepositoryOptions::with_environment(env);
    Repository::discover(Some(wt), &options).expect("discover")
}

fn expect_rev_parse_variant(repo: &Repository, spec: &str, pred: fn(&RevParseError) -> bool) {
    let err = repo
        .revwalk()
        .resolve(
            spec,
            &grit_lib::revwalk::ResolveOptions::default()
                .with_index_dwim(grit_lib::revwalk::IndexDwim::Off)
                .with_purpose(grit_lib::revwalk::ResolvePurpose::RangeEnd),
        )
        .expect_err(spec);
    let Error::Revision(RevisionError::Parse(inner)) = err else {
        panic!("{spec}: expected RevParse error, got {err}");
    };
    assert!(pred(&inner), "{spec}: unexpected variant {inner:?}");
}

#[test]
fn index_stage_errors_match_expected_variants() {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    commit_file(wt, "tracked.txt", "one\n", "one");
    let repo = open_grit_at(wt, wt);
    oracle_one(&repo, wt, ":tracked.txt", ResolveMode::Verify).unwrap_or_else(|e| panic!("{e}"));
    expect_rev_parse_variant(&repo, ":2:tracked.txt", |e| {
        matches!(e, RevParseError::PathWrongIndexStage { .. })
    });
    expect_rev_parse_variant(&repo, ":missing.txt", |e| {
        matches!(
            e,
            RevParseError::PathMissingFromWorktreeAndIndex { .. }
                | RevParseError::PathNotInIndex { .. }
        )
    });
}

#[test]
fn untracked_on_disk_colon_path_diagnose() {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    commit_file(wt, "in-repo.txt", "v\n", "in");
    fs::write(wt.join("only-disk.txt"), b"disk\n").expect("write");
    let repo = open_grit_at(wt, wt);
    expect_rev_parse_variant(&repo, ":only-disk.txt", |e| {
        matches!(e, RevParseError::PathOnDiskNotInIndex { .. })
    });
}

#[test]
fn subdirectory_prefix_index_dwim() {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    commit_file(wt, "nested/leaf.txt", "leaf\n", "nested");
    let sub = wt.join("nested");
    let repo = open_grit_at(wt, &sub);
    oracle_one(&repo, wt, ":0:leaf.txt", ResolveMode::Verify).unwrap_or_else(|e| panic!("{e}"));
    oracle_one(&repo, wt, ":leaf.txt", ResolveMode::Verify).unwrap_or_else(|e| panic!("{e}"));
}

#[test]
fn treeish_missing_path_diagnose() {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    commit_file(wt, "present.txt", "p\n", "present");
    fs::write(wt.join("absent-on-disk.txt"), b"x\n").expect("write");
    let repo = open_grit_at(wt, wt);
    expect_rev_parse_variant(&repo, "HEAD:missing-in-tree.txt", |e| {
        matches!(
            e,
            RevParseError::PathNotInRevision { .. } | RevParseError::PathOnDiskNotInRevision { .. }
        )
    });
    let _ = repo.revwalk().blob_at_path("HEAD:present.txt");
}

#[test]
fn conflict_stages_oracle_all_resolvers() {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    commit_file(wt, "file.txt", "base\n", "base");
    git(wt, &["checkout", "-b", "side"]);
    commit_file(wt, "file.txt", "side\n", "side");
    git(wt, &["checkout", "main"]);
    commit_file(wt, "file.txt", "main\n", "main");
    git_merge_expect_conflict(wt, "side");
    let repo = open_grit_at(wt, wt);
    for spec in [":1:file.txt", ":2:file.txt", ":3:file.txt"] {
        oracle_one(&repo, wt, spec, ResolveMode::Verify).unwrap_or_else(|e| panic!("{spec}: {e}"));
    }
    assert!(
        git_rev_parse_verify(wt, ":0:file.txt").is_none(),
        "git rejects stage 0 while unmerged"
    );
    expect_rev_parse_variant(&repo, ":0:file.txt", |_| true);
}

#[test]
fn remote_shorthand_stash_and_outside_paths() {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    commit_file(wt, "f.txt", "1\n", "one");
    let bare = tempfile::tempdir().expect("bare");
    git(bare.path(), &["init", "--bare", "-q"]);
    git(
        wt,
        &[
            "remote",
            "add",
            "origin",
            bare.path().to_str().expect("utf8"),
        ],
    );
    git(wt, &["push", "-u", "origin", "main"]);
    git(wt, &["fetch", "origin"]);
    let repo = open_grit_at(wt, wt);
    for spec in [
        "remotes/origin/main",
        "origin",
        "stash",
        "HEAD:/etc/passwd",
        ":0:/etc/passwd",
        ":2:/etc/passwd",
    ] {
        let _ = repo.revwalk().resolve(
            spec,
            &grit_lib::revwalk::ResolveOptions::default()
                .with_index_dwim(grit_lib::revwalk::IndexDwim::Off)
                .with_purpose(grit_lib::revwalk::ResolvePurpose::RangeEnd),
        );
        let _ = repo.revwalk().blob_at_path(spec);
    }
    assert_eq!(abbreviate_ref_name("refs/heads/main"), "main");
    assert!(upstream_suffix_info("main@{u}").is_some());
    assert!(upstream_suffix_info("main@{push}").is_some());
    let _ = repo.revwalk().upstream_symbolic_name("main@{u}");
    let _ = repo.revwalk().upstream_symbolic_name("main@{push}");
}

#[test]
fn at_minus_and_reflog_edge_specs() {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    commit_file(wt, "f.txt", "1\n", "one");
    git(wt, &["checkout", "-b", "branch-a"]);
    commit_file(wt, "f.txt", "2\n", "two");
    git(wt, &["checkout", "main"]);
    git(wt, &["checkout", "-b", "branch-b"]);
    commit_file(wt, "f.txt", "3\n", "three");
    git(wt, &["checkout", "main"]);
    let repo = open_grit_at(wt, wt);
    let _ = repo
        .revwalk()
        .resolve("@{-1}", &grit_lib::revwalk::ResolveOptions::default());
    let _ = repo
        .revwalk()
        .resolve("@{-2}", &grit_lib::revwalk::ResolveOptions::default());
    let _ = repo.revwalk().resolve(
        "main@{999}",
        &grit_lib::revwalk::ResolveOptions::default()
            .with_index_dwim(grit_lib::revwalk::IndexDwim::Off)
            .with_purpose(grit_lib::revwalk::ResolvePurpose::RangeEnd),
    );
    let _ = repo.revwalk().resolve(
        "main@{0}",
        &grit_lib::revwalk::ResolveOptions::default()
            .with_index_dwim(grit_lib::revwalk::IndexDwim::Off)
            .with_purpose(grit_lib::revwalk::ResolvePurpose::RangeEnd),
    );
    oracle_one(&repo, wt, "main@{1}", ResolveMode::Verify).unwrap_or_else(|e| panic!("{e}"));
}
