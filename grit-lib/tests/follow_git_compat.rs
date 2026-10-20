//! `RevWalk::follow` vs `git log --follow`.

mod support;

use std::path::Path;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::ObjectId;
use grit_lib::repo::Repository;
use grit_lib::revwalk::follow::{FollowWalkOptions, FollowedCommit};
use grit_lib::revwalk::RevWalkListOptions;

use support::{git, open_grit_repo, HistoryRepo};

fn git_follow_hashes(wt: &Path, path: &str, limit: Option<usize>) -> Vec<ObjectId> {
    let mut args = vec!["log", "--follow", "--format=%H"];
    let limit_str;
    if let Some(n) = limit {
        limit_str = n.to_string();
        args.push("-n");
        args.push(&limit_str);
    }
    args.push("--");
    args.push(path);
    git(wt, &args)
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| l.parse().expect("hex"))
        .collect()
}

fn grit_follow_hashes(
    wt: &Path,
    path: &str,
    limit: Option<usize>,
    first_parent: bool,
) -> Vec<ObjectId> {
    let repo = open_grit_repo(wt);
    let opts = FollowWalkOptions {
        walk: RevWalkListOptions {
            limit,
            first_parent,
            ..RevWalkListOptions::default()
        },
        ..FollowWalkOptions::default()
    };
    let walked = repo
        .rev_walk()
        .follow(&["HEAD"], path, &opts)
        .expect("follow");
    walked.map(|c| c.oid).collect()
}

fn assert_follow_matches_git(wt: &Path, path: &str, limit: Option<usize>, first_parent: bool) {
    let git_list = git_follow_hashes(wt, path, limit);
    let grit_list = grit_follow_hashes(wt, path, limit, first_parent);
    assert_eq!(
        grit_list, git_list,
        "follow mismatch for {path:?} limit={limit:?} first_parent={first_parent}"
    );
}

#[test]
fn follow_simple_rename_matches_git() {
    let repo = HistoryRepo::init();
    let worktree = repo.worktree();
    git(worktree, &["config", "user.email", "t@e"]);
    git(&worktree, &["config", "user.name", "T"]);
    std::fs::write(worktree.join("a.txt"), "v1\n").unwrap();
    git(&worktree, &["add", "a.txt"]);
    git(&worktree, &["commit", "-m", "add"]);
    git(&worktree, &["mv", "a.txt", "b.txt"]);
    git(&worktree, &["commit", "-m", "rename"]);
    assert_follow_matches_git(&worktree, "b.txt", None, false);
}

#[test]
fn follow_rename_with_edits_matches_git() {
    let repo = HistoryRepo::init();
    let worktree = repo.worktree();
    git(worktree, &["config", "user.email", "t@e"]);
    git(&worktree, &["config", "user.name", "T"]);
    std::fs::write(worktree.join("f.txt"), "one\n").unwrap();
    git(&worktree, &["add", "f.txt"]);
    git(&worktree, &["commit", "-m", "c1"]);
    std::fs::write(worktree.join("f.txt"), "two\n").unwrap();
    git(&worktree, &["add", "f.txt"]);
    git(&worktree, &["commit", "-m", "c2"]);
    git(&worktree, &["mv", "f.txt", "g.txt"]);
    git(&worktree, &["commit", "-m", "rename"]);
    std::fs::write(worktree.join("g.txt"), "three\n").unwrap();
    git(&worktree, &["add", "g.txt"]);
    git(&worktree, &["commit", "-m", "c3"]);
    assert_follow_matches_git(&worktree, "g.txt", None, false);
}

#[test]
fn follow_rename_chain_matches_git() {
    let repo = HistoryRepo::init();
    let worktree = repo.worktree();
    git(worktree, &["config", "user.email", "t@e"]);
    git(&worktree, &["config", "user.name", "T"]);
    std::fs::write(worktree.join("a.txt"), "x\n").unwrap();
    git(&worktree, &["add", "a.txt"]);
    git(&worktree, &["commit", "-m", "c1"]);
    git(&worktree, &["mv", "a.txt", "b.txt"]);
    git(&worktree, &["commit", "-m", "r1"]);
    git(&worktree, &["mv", "b.txt", "c.txt"]);
    git(&worktree, &["commit", "-m", "r2"]);
    assert_follow_matches_git(&worktree, "c.txt", None, false);
}

#[test]
fn follow_across_merge_matches_git() {
    let repo = HistoryRepo::init();
    let worktree = repo.worktree();
    git(worktree, &["config", "user.email", "t@e"]);
    git(&worktree, &["config", "user.name", "T"]);
    std::fs::write(worktree.join("f.txt"), "base\n").unwrap();
    git(&worktree, &["add", "f.txt"]);
    git(&worktree, &["commit", "-m", "root"]);
    git(&worktree, &["checkout", "-q", "-b", "side"]);
    std::fs::write(worktree.join("side.txt"), "s\n").unwrap();
    git(&worktree, &["add", "side.txt"]);
    git(&worktree, &["commit", "-m", "side"]);
    git(&worktree, &["checkout", "-q", "main"]);
    std::fs::write(worktree.join("f.txt"), "main\n").unwrap();
    git(&worktree, &["add", "f.txt"]);
    git(&worktree, &["commit", "-m", "edit"]);
    git(&worktree, &["mv", "f.txt", "g.txt"]);
    git(&worktree, &["commit", "-m", "rename"]);
    git(&worktree, &["merge", "-q", "-m", "merge", "side"]);
    assert_follow_matches_git(&worktree, "g.txt", None, false);
}

#[test]
fn follow_with_max_count_matches_git() {
    let repo = HistoryRepo::init();
    let worktree = repo.worktree();
    git(worktree, &["config", "user.email", "t@e"]);
    git(&worktree, &["config", "user.name", "T"]);
    for i in 0..5 {
        std::fs::write(worktree.join("a.txt"), format!("v{i}\n")).unwrap();
        git(&worktree, &["add", "a.txt"]);
        git(&worktree, &["commit", "-m", &format!("c{i}")]);
    }
    git(&worktree, &["mv", "a.txt", "b.txt"]);
    git(&worktree, &["commit", "-m", "rename"]);
    assert_follow_matches_git(&worktree, "b.txt", Some(3), false);
}

#[test]
fn follow_rejects_multiple_paths() {
    let repo = HistoryRepo::init();
    let worktree = repo.worktree();
    let grit = Repository::discover(Some(worktree), &RepositoryOptions::empty()).unwrap();
    match grit.rev_walk().follow(
        &["HEAD"],
        "a.txt",
        &FollowWalkOptions {
            walk: RevWalkListOptions {
                paths: vec!["b.txt".into()],
                ..Default::default()
            },
            ..Default::default()
        },
    ) {
        Ok(_) => panic!("expected follow to reject multiple paths"),
        Err(err) => assert!(err.to_string().contains("one path"), "unexpected: {err}"),
    }
}
