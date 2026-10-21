//! Pickaxe (`-S` / `-G`) vs system `git log`.

mod support;

use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use grit_lib::diff::Pickaxe;
use grit_lib::revwalk::rev_list::{rev_list, RevListOptions};

use support::{commit_file, git, open_grit_repo, HistoryRepo};

fn git_log_pickaxe(dir: &Path, args: &[&str]) -> Vec<String> {
    let mut argv = vec!["log", "--format=%H"];
    argv.extend(args);
    git(dir, &argv)
        .lines()
        .filter(|l| !l.is_empty())
        .map(str::to_owned)
        .collect()
}

fn grit_pickaxe_log(
    dir: &Path,
    pickaxe: Arc<Pickaxe>,
    max_count: Option<usize>,
    paths: Vec<String>,
) -> Vec<String> {
    let repo = open_grit_repo(dir);
    let opts = RevListOptions {
        max_count,
        paths,
        pickaxe: Some(pickaxe),
        use_commit_graph: true,
        ..RevListOptions::default()
    };
    rev_list(&repo, &["HEAD".into()], &[], &opts)
        .expect("rev-list")
        .commits
        .into_iter()
        .map(|oid| oid.to_hex())
        .collect()
}

fn setup_basic_pickaxe_repo(dir: &Path) -> (String, String) {
    std::fs::write(dir.join("file"), "").expect("file");
    git(dir, &["add", "file"]);
    git(dir, &["commit", "-m", "initial"]);
    let initial = git(dir, &["rev-parse", "HEAD"]);
    std::fs::write(dir.join("file"), "Picked\n").expect("file");
    git(dir, &["add", "file"]);
    Command::new("git")
        .current_dir(dir)
        .args([
            "commit",
            "-m",
            "second",
            "--author=Another Person <another@example.com>",
        ])
        .env("GIT_AUTHOR_DATE", "@1000 +0000")
        .env("GIT_COMMITTER_DATE", "@1000 +0000")
        .env("GIT_CONFIG_GLOBAL", support::null_device_for_tests())
        .env("GIT_CONFIG_SYSTEM", support::null_device_for_tests())
        .status()
        .expect("commit");
    let second = git(dir, &["rev-parse", "HEAD"]).trim().to_string();
    (initial.trim().to_string(), second)
}

#[test]
fn pickaxe_s_matches_git() {
    let repo = HistoryRepo::init();
    let worktree = repo.worktree();
    let (_initial, second) = setup_basic_pickaxe_repo(worktree);
    let git_oids = git_log_pickaxe(&worktree, &["-S", "Picked"]);
    let grit_oids = grit_pickaxe_log(
        &worktree,
        Pickaxe::occurrences("Picked").into_shared(),
        None,
        Vec::new(),
    );
    assert_eq!(git_oids, grit_oids);
    assert_eq!(grit_oids, vec![second]);
}

#[test]
fn pickaxe_s_moved_within_file_not_reported_matches_git() {
    let repo = HistoryRepo::init();
    let worktree = repo.worktree();
    std::fs::write(worktree.join("f.txt"), "aaa\nbbb\nccc\n").expect("write");
    git(&worktree, &["add", "f.txt"]);
    git(&worktree, &["commit", "-m", "one"]);
    std::fs::write(worktree.join("f.txt"), "bbb\naaa\nccc\n").expect("write");
    git(&worktree, &["add", "f.txt"]);
    git(&worktree, &["commit", "-m", "move"]);
    let git_oids = git_log_pickaxe(&worktree, &["-S", "aaa"]);
    let grit_oids = grit_pickaxe_log(
        &worktree,
        Pickaxe::occurrences("aaa").into_shared(),
        None,
        Vec::new(),
    );
    assert_eq!(git_oids, grit_oids);
}

#[test]
fn pickaxe_g_matches_git() {
    let repo = HistoryRepo::init();
    let worktree = repo.worktree();
    let (_initial, second) = setup_basic_pickaxe_repo(&worktree);
    let git_oids = git_log_pickaxe(&worktree, &["-G", "Picked"]);
    let grit_oids = grit_pickaxe_log(
        &worktree,
        Pickaxe::line_grep("Picked", false)
            .expect("grep")
            .into_shared(),
        None,
        Vec::new(),
    );
    assert_eq!(git_oids, grit_oids);
    assert_eq!(grit_oids, vec![second]);
}

#[test]
fn pickaxe_ignore_case_matches_git() {
    let repo = HistoryRepo::init();
    let worktree = repo.worktree();
    let (_initial, second) = setup_basic_pickaxe_repo(&worktree);
    for extra in ["-i", "--regexp-ignore-case"] {
        let git_oids = git_log_pickaxe(&worktree, &["-S", "picked", extra]);
        let grit_oids = grit_pickaxe_log(
            &worktree,
            Pickaxe::occurrences("picked")
                .ignore_case(true)
                .into_shared(),
            None,
            Vec::new(),
        );
        assert_eq!(git_oids, grit_oids, "flag {extra}");
        assert_eq!(grit_oids, vec![second.clone()]);
    }
}

#[test]
fn pickaxe_g_ere_alternation_matches_git() {
    let repo = HistoryRepo::init();
    let worktree = repo.worktree();
    commit_file(worktree, "data.txt", "a\n", "A");
    let add_foo = commit_file(worktree, "data.txt", "foo\n", "add-foo");
    let git_oids = git_log_pickaxe(&worktree, &["-G", "foo|bar"]);
    let grit_oids = grit_pickaxe_log(
        &worktree,
        Pickaxe::line_grep("foo|bar", false)
            .expect("grep")
            .into_shared(),
        None,
        Vec::new(),
    );
    assert_eq!(git_oids, grit_oids);
    assert_eq!(grit_oids, vec![add_foo.to_hex()]);
}

#[test]
fn pickaxe_s_pickaxe_regex_leftmost_longest_alternation_matches_git() {
    let repo = HistoryRepo::init();
    let worktree = repo.worktree();
    let root = commit_file(worktree, "data.txt", "aa\n", "root-aa");
    commit_file(worktree, "data.txt", "a\n", "change-to-a");
    let git_oids = git_log_pickaxe(&worktree, &["-S", "a|aa", "--pickaxe-regex"]);
    let grit_oids = grit_pickaxe_log(
        &worktree,
        Pickaxe::occurrences("a|aa")
            .pickaxe_regex(true)
            .build()
            .expect("pickaxe")
            .into_shared(),
        None,
        Vec::new(),
    );
    assert_eq!(git_oids, grit_oids);
    assert_eq!(git_oids, vec![root.to_hex()]);
}

#[test]
fn pickaxe_s_pickaxe_regex_grouping_matches_git() {
    let repo = HistoryRepo::init();
    let worktree = repo.worktree();
    commit_file(worktree, "data.txt", "aa\n", "root-aa");
    commit_file(worktree, "data.txt", "a\n", "change-to-a");
    let git_oids = git_log_pickaxe(&worktree, &["-S", "(a|aa)", "--pickaxe-regex"]);
    let grit_oids = grit_pickaxe_log(
        &worktree,
        Pickaxe::occurrences("(a|aa)")
            .pickaxe_regex(true)
            .build()
            .expect("pickaxe")
            .into_shared(),
        None,
        Vec::new(),
    );
    assert_eq!(git_oids, grit_oids);
}

#[test]
fn pickaxe_s_pickaxe_regex_ere_matches_git() {
    let repo = HistoryRepo::init();
    let worktree = repo.worktree();
    commit_file(worktree, "data.txt", "a\n", "A");
    let add_foo = commit_file(worktree, "data.txt", "foo\n", "add-foo");
    let git_oids = git_log_pickaxe(&worktree, &["-S", "foo|bar", "--pickaxe-regex"]);
    let grit_oids = grit_pickaxe_log(
        &worktree,
        Pickaxe::occurrences("foo|bar")
            .pickaxe_regex(true)
            .build()
            .expect("pickaxe")
            .into_shared(),
        None,
        Vec::new(),
    );
    assert_eq!(git_oids, grit_oids);
    assert_eq!(grit_oids, vec![add_foo.to_hex()]);
}

#[test]
fn pickaxe_merge_default_skipped_matches_git() {
    let repo = HistoryRepo::init();
    let worktree = repo.worktree();
    commit_file(worktree, "f.txt", "root\n", "root");
    git(worktree, &["checkout", "-b", "side"]);
    commit_file(worktree, "f.txt", "base\n", "base");
    commit_file(worktree, "f.txt", "base\nneedle\n", "side-needle");
    git(worktree, &["checkout", "main"]);
    git(worktree, &["checkout", "-b", "main2"]);
    commit_file(worktree, "other.txt", "x\n", "main-touch");
    git(worktree, &["checkout", "main"]);
    git(worktree, &["merge", "side", "-m", "merge side"]);
    let git_oids = git_log_pickaxe(&worktree, &["-S", "needle"]);
    let grit_oids = grit_pickaxe_log(
        &worktree,
        Pickaxe::occurrences("needle").into_shared(),
        None,
        Vec::new(),
    );
    assert_eq!(git_oids, grit_oids);
    assert!(
        !git_oids.is_empty(),
        "expected side commit with needle introduction"
    );
    assert_eq!(git_oids.len(), 1, "merge commit must not appear by default");
}

#[test]
fn pickaxe_with_paths_and_max_count_matches_git() {
    let repo = HistoryRepo::init();
    let worktree = repo.worktree();
    commit_file(&worktree, "a.txt", "alpha\n", "a");
    commit_file(&worktree, "b.txt", "beta picked\n", "b");
    commit_file(&worktree, "a.txt", "alpha picked\n", "c");
    let git_oids = git_log_pickaxe(&worktree, &["-S", "picked", "-n", "1", "--", "a.txt"]);
    let grit_oids = grit_pickaxe_log(
        &worktree,
        Pickaxe::occurrences("picked").into_shared(),
        Some(1),
        vec!["a.txt".into()],
    );
    assert_eq!(git_oids, grit_oids);
    assert_eq!(git_oids.len(), 1);
}
