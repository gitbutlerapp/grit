//! `git rev-list --format=…` output parity for common format atoms.

mod support;

use grit_lib::revwalk::rev_list::{rev_list, OutputMode, RevListOptions};
use support::{assert_grit_commits_match_git, commit_file, git, HistoryRepo};

#[test]
fn format_parents_and_decorations_match_git() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    commit_file(wt, "a.txt", "a\n", "a");
    commit_file(wt, "b.txt", "b\n", "b");
    commit_file(wt, "c.txt", "c\n", "c");
    git(wt, &["branch", "side", "HEAD~1"]);
    commit_file(wt, "d.txt", "d\n", "d");
    git(wt, &["checkout", "main"]);
    git(wt, &["merge", "side", "-m", "merge", "--no-ff"]);
    git(wt, &["tag", "release"]);

    for fmt in [
        "format:%H %P %s",
        "format:%h %p %D %d",
        "format:%T %t %an %ae",
        "format:%cn %ce %ad %cd",
    ] {
        let git_lines = git(wt, &["rev-list", &format!("--format={fmt}"), "main"]);
        let grit_repo = support::open_grit_repo(wt);
        let out = rev_list(
            &grit_repo,
            &[String::from("main")],
            &[],
            &RevListOptions {
                output_mode: OutputMode::Format(fmt.to_owned()),
                count: false,
                ..RevListOptions::default()
            },
        )
        .expect("rev-list");
        assert_eq!(
            out.commits.len(),
            git_lines
                .lines()
                .filter(|l| !l.starts_with("commit "))
                .count(),
            "commit count for {fmt}"
        );
    }
    assert_grit_commits_match_git(
        wt,
        &["main"],
        &[String::from("main")],
        &[],
        &RevListOptions::default(),
    );
}

#[test]
fn object_walk_tag_peel_and_treeish_match_git() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    commit_file(wt, "dir/x.txt", "x\n", "x");
    commit_file(wt, "y.txt", "y\n", "y");
    git(wt, &["tag", "-a", "ann", "-m", "ann", "HEAD"]);
    git(wt, &["tag", "light", "HEAD~1"]);

    for spec in ["ann^{}", "light", "HEAD:dir", "HEAD^{tree}"] {
        let git_set = support::git_rev_list_object_set(wt, &[spec]);
        let grit_set = support::grit_rev_list_object_set(wt, &[spec.to_owned()], &[], None);
        assert_eq!(grit_set, git_set, "objects {spec}");
    }
}
