//! `git rev-list --parents` walk parity: parent rewriting lists the same commits as Git.

use crate::support;

use grit_lib::revwalk::rev_list::RevListOptions;
use support::{assert_grit_commits_match_git, commit_file, git, HistoryRepo};

#[test]
fn parent_rewrite_walk_matches_git() {
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

    let parents = RevListOptions {
        parent_rewrite: true,
        ..RevListOptions::default()
    };
    assert_grit_commits_match_git(
        wt,
        &["--parents", "main"],
        &[String::from("main")],
        &[],
        &parents,
    );
    assert_grit_commits_match_git(
        wt,
        &["--parents", "main", "--", "b.txt"],
        &[String::from("main")],
        &[],
        &RevListOptions {
            paths: vec![String::from("b.txt")],
            ..parents.clone()
        },
    );
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
