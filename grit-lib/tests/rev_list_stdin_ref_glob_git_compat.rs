//! Upstream-style `--stdin` ref pseudo-options (`--branches`, `--tags`, `--remotes`, `--glob`).
//! Behavioral parity vs `git rev-list --stdin` (t6017 / t6018 extensions).

mod support;

use grit_lib::revwalk::rev_list::{
    collect_revision_specs_from_stdin_lines, rev_list, RevListOptions,
};
use support::{assert_grit_commits_match_git, commit_file, git, HistoryRepo};

fn git_stdin_rev_list(
    worktree: &std::path::Path,
    lines: &[&str],
) -> Vec<grit_lib::objects::ObjectId> {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let mut child = Command::new("git")
        .current_dir(worktree)
        .args(["rev-list", "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .spawn()
        .expect("git rev-list --stdin");
    {
        let stdin = child.stdin.as_mut().expect("stdin");
        for line in lines {
            writeln!(stdin, "{line}").expect("write line");
        }
    }
    let out = child.wait_with_output().expect("wait");
    assert!(
        out.status.success(),
        "git rev-list --stdin: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.parse().ok())
        .collect()
}

fn grit_stdin_rev_list(
    worktree: &std::path::Path,
    args_specs: &[String],
    lines: &[&str],
    opts: &RevListOptions,
) -> Vec<grit_lib::objects::ObjectId> {
    let grit_repo = grit_lib::repo::Repository::discover(
        Some(worktree),
        &grit_lib::config::environment::RepositoryOptions::empty(),
    )
    .expect("discover");
    let (pos, neg, all_refs, paths) =
        collect_revision_specs_from_stdin_lines(&grit_repo, args_specs, lines)
            .expect("stdin specs");
    let mut walk_opts = opts.clone();
    if all_refs {
        walk_opts.all_refs = true;
    }
    if !paths.is_empty() {
        walk_opts.paths = paths;
    }
    rev_list(&grit_repo, &pos, &neg, &walk_opts)
        .expect("rev-list")
        .commits
}

fn setup_multi_ref_repo(wt: &std::path::Path) -> (String, String) {
    commit_file(wt, "main.txt", "main\n", "on main");
    let main_tip = git(wt, &["rev-parse", "HEAD"]).trim().to_string();
    git(wt, &["branch", "feature/one"]);
    commit_file(wt, "feature.txt", "feature\n", "on feature");
    let feature_tip = git(wt, &["rev-parse", "HEAD"]).trim().to_string();
    git(wt, &["checkout", "main"]);
    git(wt, &["tag", "release-1"]);
    std::fs::create_dir_all(wt.join(".git/refs/remotes/origin")).expect("remotes dir");
    std::fs::write(
        wt.join(".git/refs/remotes/origin/main"),
        format!("{main_tip}\n"),
    )
    .expect("remote ref");
    (main_tip, feature_tip)
}

#[test]
fn stdin_branches_tags_remotes_match_git() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    setup_multi_ref_repo(wt);

    for (lines, ctx) in [
        (vec!["--branches"], "stdin --branches"),
        (vec!["--tags"], "stdin --tags"),
        (vec!["--remotes"], "stdin --remotes"),
    ] {
        let git_oids = git_stdin_rev_list(wt, &lines);
        let grit_oids = grit_stdin_rev_list(wt, &[], &lines, &RevListOptions::default());
        assert_eq!(grit_oids, git_oids, "{ctx}");
    }
}

#[test]
fn stdin_glob_and_tag_patterns_match_git() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    setup_multi_ref_repo(wt);

    let cases: &[(&[&str], &str)] = &[
        (&["--glob=refs/heads/feature/*"], "glob feature branches"),
        (&["--tags=release*"], "tags release*"),
        (&["--glob=refs/remotes/origin/*"], "glob origin remote"),
    ];
    for (lines, ctx) in cases {
        let git_oids = git_stdin_rev_list(wt, lines);
        let grit_oids = grit_stdin_rev_list(wt, &[], lines, &RevListOptions::default());
        assert_eq!(grit_oids, git_oids, "{ctx}");
    }
}

#[test]
fn stdin_not_all_expands_negative_refs() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    setup_multi_ref_repo(wt);
    let lines = ["--not", "--all"];
    let grit_repo = grit_lib::repo::Repository::discover(
        Some(wt),
        &grit_lib::config::environment::RepositoryOptions::empty(),
    )
    .expect("discover");
    let (_pos, neg, all, _paths) = collect_revision_specs_from_stdin_lines(&grit_repo, &[], &lines)
        .expect("stdin --not --all specs");
    assert!(!neg.is_empty(), "expected negative refs from --not --all");
    assert!(!all, "--not --all must not set all_refs positive mode");
}

#[test]
fn stdin_not_branches_matches_git_empty() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    setup_multi_ref_repo(wt);
    let lines = ["--not", "--branches"];
    let git_oids = git_stdin_rev_list(wt, &lines);
    let grit_oids = grit_stdin_rev_list(wt, &[], &lines, &RevListOptions::default());
    assert_eq!(grit_oids, git_oids, "stdin --not --branches");
}

#[test]
fn stdin_end_of_options_and_path_tail_match_git() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    setup_multi_ref_repo(wt);
    commit_file(wt, "scoped.txt", "scoped\n", "scoped file");
    let lines = ["--end-of-options", "HEAD", "--", "scoped.txt"];
    let git_oids = git_stdin_rev_list(wt, &lines);
    let grit_oids = grit_stdin_rev_list(wt, &[], &lines, &RevListOptions::default());
    assert_eq!(grit_oids, git_oids, "stdin --end-of-options with path tail");
}

#[test]
fn stdin_cli_tip_intersect_branches_match_git() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    setup_multi_ref_repo(wt);
    let lines = ["--branches"];
    let git_stdin = git_stdin_rev_list(wt, &lines);
    let grit_oids = grit_stdin_rev_list(
        wt,
        &[String::from("HEAD")],
        &lines,
        &RevListOptions::default(),
    );
    assert_eq!(grit_oids, git_stdin, "HEAD intersect stdin --branches");
    assert_grit_commits_match_git(
        wt,
        &["--branches"],
        &git_stdin.iter().map(|o| o.to_hex()).collect::<Vec<_>>(),
        &[],
        &RevListOptions::default(),
    );
}
