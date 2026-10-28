//! Pickaxe (`-S` / `-G`) vs system `git log`.

use crate::support;

use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use grit_lib::config::ConfigSet;
use grit_lib::diff::{Pickaxe, RenameDetection};
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
fn pickaxe_s_gitlink_with_max_count_matches_git() {
    let repo = HistoryRepo::init();
    let worktree = repo.worktree();
    std::fs::write(worktree.join("f"), "a\n").expect("write f");
    git(worktree, &["add", "f"]);
    git(
        worktree,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            "160000,1111111111111111111111111111111111111111,lib",
        ],
    );
    git(worktree, &["commit", "-m", "one"]);
    std::fs::write(worktree.join("f"), "b\n").expect("write f");
    git(worktree, &["add", "f"]);
    git(
        worktree,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            "160000,2222222222222222222222222222222222222222,lib",
        ],
    );
    git(worktree, &["commit", "-m", "two"]);
    let git_oids = git_log_pickaxe(worktree, &["-S", "anything", "-n", "10"]);
    let grit_oids = grit_pickaxe_log(
        worktree,
        Pickaxe::occurrences("anything").into_shared(),
        Some(10),
        Vec::new(),
    );
    assert_eq!(git_oids, grit_oids);
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

const NEEDLE_FILE: &str = "filler line 1\nfiller line 2\nfiller line 3\nfiller line 4\n\
filler line 5\nfiller line 6\nfiller line 7\nfiller line 8\nfiller line 9\nfiller line 10\n\
filler line 11\nfiller line 12\nfiller line 13\nfiller line 14\nfiller line 15\n\
needle one\nneedle two\n";

/// Add `from` with two needles, rename it to `to` while appending `rename_extra`, then add a
/// third needle (issue #2203). Returns the three commit ids, oldest first.
fn rename_history(worktree: &Path, from: &str, to: &str, rename_extra: &str) -> [String; 3] {
    let add = commit_file(worktree, from, NEEDLE_FILE, "add").to_hex();
    if let Some(parent) = worktree.join(to).parent() {
        std::fs::create_dir_all(parent).expect("mkdir");
    }
    git(worktree, &["mv", from, to]);
    let renamed = format!("{NEEDLE_FILE}{rename_extra}");
    let rename = commit_file(worktree, to, &renamed, "rename").to_hex();
    let needle = commit_file(worktree, to, &format!("{renamed}needle three\n"), "needle").to_hex();
    [add, rename, needle]
}

fn assert_pickaxe_matches_git(
    worktree: &Path,
    git_args: &[&str],
    pickaxe: Arc<Pickaxe>,
    max_count: Option<usize>,
    paths: &[&str],
) -> Vec<String> {
    let mut argv: Vec<&str> = git_args.to_vec();
    let max_arg = max_count.map(|n| n.to_string());
    if let Some(n) = max_arg.as_deref() {
        argv.extend(["-n", n]);
    }
    if !paths.is_empty() {
        argv.push("--");
        argv.extend(paths);
    }
    let git_oids = git_log_pickaxe(worktree, &argv);
    let grit_oids = grit_pickaxe_log(
        worktree,
        pickaxe,
        max_count,
        paths.iter().map(|p| (*p).to_owned()).collect(),
    );
    assert_eq!(grit_oids, git_oids, "git log {argv:?}");
    git_oids
}

fn needle_s() -> Arc<Pickaxe> {
    Pickaxe::occurrences("needle").into_shared()
}

fn needle_g() -> Arc<Pickaxe> {
    Pickaxe::line_grep("needle", false)
        .expect("grep")
        .into_shared()
}

#[test]
fn pickaxe_pure_rename_is_not_a_change_matches_git() {
    let repo = HistoryRepo::init();
    let worktree = repo.worktree();
    let [add, _rename, needle] = rename_history(worktree, "a.txt", "b.txt", "extra line\n");
    for max_count in [None, Some(10)] {
        let s = assert_pickaxe_matches_git(worktree, &["-S", "needle"], needle_s(), max_count, &[]);
        assert_eq!(s, vec![needle.clone(), add.clone()]);
        let g = assert_pickaxe_matches_git(worktree, &["-G", "needle"], needle_g(), max_count, &[]);
        assert_eq!(g, vec![needle.clone(), add.clone()]);
    }
}

#[test]
fn pickaxe_rename_with_renames_disabled_matches_git() {
    let repo = HistoryRepo::init();
    let worktree = repo.worktree();
    let [add, rename, needle] = rename_history(worktree, "a.txt", "b.txt", "extra line\n");
    git(worktree, &["config", "diff.renames", "false"]);
    for max_count in [None, Some(10)] {
        let s = assert_pickaxe_matches_git(worktree, &["-S", "needle"], needle_s(), max_count, &[]);
        assert_eq!(s, vec![needle.clone(), rename.clone(), add.clone()]);
        let g = assert_pickaxe_matches_git(worktree, &["-G", "needle"], needle_g(), max_count, &[]);
        assert_eq!(g.len(), 3);
    }
}

#[test]
fn pickaxe_rename_with_pathspec_on_new_path_only_matches_git() {
    let repo = HistoryRepo::init();
    let worktree = repo.worktree();
    let [_add, rename, needle] = rename_history(worktree, "a.txt", "b.txt", "extra line\n");
    for max_count in [None, Some(10)] {
        let s = assert_pickaxe_matches_git(
            worktree,
            &["-S", "needle"],
            needle_s(),
            max_count,
            &["b.txt"],
        );
        assert_eq!(s, vec![needle.clone(), rename.clone()]);
        assert_pickaxe_matches_git(
            worktree,
            &["-G", "needle"],
            needle_g(),
            max_count,
            &["b.txt"],
        );
    }
}

#[test]
fn pickaxe_move_into_pathspec_directory_matches_git() {
    let repo = HistoryRepo::init();
    let worktree = repo.worktree();
    let [_add, rename, needle] = rename_history(worktree, "a.txt", "sub/a.txt", "extra line\n");
    for max_count in [None, Some(10)] {
        let s = assert_pickaxe_matches_git(
            worktree,
            &["-S", "needle"],
            needle_s(),
            max_count,
            &["sub"],
        );
        assert_eq!(s, vec![needle.clone(), rename.clone()]);
        assert_pickaxe_matches_git(worktree, &["-G", "needle"], needle_g(), max_count, &["sub"]);
    }
}

#[test]
fn pickaxe_rename_within_pathspec_directory_matches_git() {
    let repo = HistoryRepo::init();
    let worktree = repo.worktree();
    commit_file(worktree, "other/x.txt", "needle elsewhere\n", "other");
    let [add, _rename, needle] = rename_history(worktree, "sub/a.c", "sub/b.c", "extra line\n");
    for max_count in [None, Some(10)] {
        for spec in ["sub", "sub/", "sub/*.c"] {
            let s = assert_pickaxe_matches_git(
                worktree,
                &["-S", "needle"],
                needle_s(),
                max_count,
                &[spec],
            );
            assert_eq!(s, vec![needle.clone(), add.clone()], "pathspec {spec}");
            let g = assert_pickaxe_matches_git(
                worktree,
                &["-G", "needle"],
                needle_g(),
                max_count,
                &[spec],
            );
            assert_eq!(g, vec![needle.clone(), add.clone()], "pathspec {spec}");
        }
    }
}

#[test]
fn pickaxe_rename_that_changes_matches_is_listed_matches_git() {
    let repo = HistoryRepo::init();
    let worktree = repo.worktree();
    let [add, rename, needle] = rename_history(worktree, "a.txt", "b.txt", "needle extra\n");
    for max_count in [None, Some(10)] {
        let s = assert_pickaxe_matches_git(worktree, &["-S", "needle"], needle_s(), max_count, &[]);
        assert_eq!(s, vec![needle.clone(), rename.clone(), add.clone()]);
        let g = assert_pickaxe_matches_git(worktree, &["-G", "needle"], needle_g(), max_count, &[]);
        assert_eq!(g, vec![needle.clone(), rename.clone(), add.clone()]);
    }
}

#[test]
fn pickaxe_g_line_anchors_on_modified_files_matches_git() {
    let repo = HistoryRepo::init();
    let worktree = repo.worktree();
    commit_file(worktree, "f.txt", "alpha\nbeta\n", "base");
    commit_file(worktree, "f.txt", "alpha\nxneedle\nbeta\n", "unanchored");
    commit_file(
        worktree,
        "f.txt",
        "alpha\nxneedle\nneedle\nbeta\n",
        "anchored",
    );
    commit_file(
        worktree,
        "f.txt",
        "alpha\nxneedle\nneedle\nbeta\nlast two",
        "no-eol",
    );
    commit_file(worktree, "g.txt", "needle\n", "other-file");
    for pattern in ["^needle$", "needle", "^x", "two$", "a$", "^beta"] {
        let pickaxe = Pickaxe::line_grep(pattern, false)
            .expect("grep")
            .into_shared();
        assert_pickaxe_matches_git(worktree, &["-G", pattern], pickaxe, None, &["f.txt"]);
    }
}

#[test]
fn pickaxe_matches_commit_pairs_renames_per_policy() {
    let repo = HistoryRepo::init();
    let worktree = repo.worktree();
    let [add, rename, needle] = rename_history(worktree, "a.txt", "sub/b.txt", "extra line\n");
    let grit = open_grit_repo(worktree);
    let config = ConfigSet::new();
    let pickaxe = Pickaxe::occurrences("needle");
    let matches = |oid: &str, paths: &[&str], renames: RenameDetection| {
        let paths: Vec<String> = paths.iter().map(|p| (*p).to_owned()).collect();
        pickaxe
            .matches_commit(&grit, oid.parse().expect("oid"), &paths, renames, &config)
            .expect("pickaxe")
    };
    let on = RenameDetection::default();
    assert!(matches(&add, &[], on));
    assert!(matches(&needle, &[], on));
    assert!(!matches(&rename, &[], on));
    assert!(matches(&rename, &[], RenameDetection::Off));
    assert!(
        matches(&rename, &["sub"], on),
        "moved in from outside the pathspec"
    );
    assert!(!matches(&rename, &["nowhere"], on));
}

#[test]
fn pickaxe_g_repeated_anchor_line_stays_context_matches_git() {
    let repo = HistoryRepo::init();
    let worktree = repo.worktree();
    let anchor = "fn unstable_config_on_stable() {\n";
    let old_body = format!("{anchor}{anchor}// before\n");
    commit_file(worktree, "f.rs", &old_body, "base");
    let mut new_body = String::new();
    for i in 0..600 {
        new_body.push_str(&format!("// block {i}\n"));
    }
    new_body.push_str(&anchor);
    new_body.push_str("// after\n");
    commit_file(worktree, "f.rs", &new_body, "big-insert");
    let pickaxe = Pickaxe::line_grep("unstable_config_on_stable", false)
        .expect("grep")
        .into_shared();
    assert_pickaxe_matches_git(worktree, &["-G", "unstable_config_on_stable"], pickaxe, None, &[]);
}

#[test]
fn pickaxe_g_moved_matching_line_matches_git() {
    let repo = HistoryRepo::init();
    let worktree = repo.worktree();
    commit_file(
        worktree,
        "f.txt",
        "header\n(unstable)\nfooter\n",
        "base",
    );
    commit_file(
        worktree,
        "f.txt",
        "header\nfooter\n(unstable)\n",
        "move-line",
    );
    let pickaxe = Pickaxe::line_grep("unstable", false)
        .expect("grep")
        .into_shared();
    assert_pickaxe_matches_git(worktree, &["-G", "unstable"], pickaxe, None, &["f.txt"]);
}

#[test]
fn pickaxe_first_parent_diffs_merges_against_first_parent_matches_git() {
    let repo = HistoryRepo::init();
    let worktree = repo.worktree();
    commit_file(worktree, "f.txt", "root\n", "root");
    git(worktree, &["checkout", "-b", "side"]);
    commit_file(worktree, "f.txt", "root\nneedle\n", "side-needle");
    git(worktree, &["checkout", "main"]);
    commit_file(worktree, "g.txt", "x\n", "main-touch");
    git(worktree, &["merge", "side", "-m", "merge side"]);
    let merge = git(worktree, &["rev-parse", "HEAD"]).trim().to_owned();
    let git_oids = git_log_pickaxe(worktree, &["--first-parent", "-S", "needle"]);
    assert_eq!(git_oids, vec![merge]);
    let repo = open_grit_repo(worktree);
    let opts = RevListOptions {
        first_parent: true,
        pickaxe: Some(needle_s()),
        use_commit_graph: true,
        ..RevListOptions::default()
    };
    let grit_oids: Vec<String> = rev_list(&repo, &["HEAD".into()], &[], &opts)
        .expect("rev-list")
        .commits
        .into_iter()
        .map(|oid| oid.to_hex())
        .collect();
    assert_eq!(grit_oids, git_oids);
}
