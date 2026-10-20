//! `RevWalk::follow` vs `git log --follow`.

mod support;

use std::path::Path;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::ObjectId;
use grit_lib::repo::Repository;
use grit_lib::revwalk::follow::{FollowWalkOptions, FollowedCommit};
use grit_lib::revwalk::RevWalkListOptions;

use support::{commit_labeled_with_dates, git, open_grit_repo, HistoryRepo};

fn git_follow_hashes(
    wt: &Path,
    path: &str,
    limit: Option<usize>,
    until: Option<&str>,
) -> Vec<ObjectId> {
    let mut args = vec!["log", "--follow", "--format=%H"];
    if let Some(u) = until {
        args.push("--until");
        args.push(u);
    }
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

fn grit_follow_hashes(wt: &Path, path: &str, walk: RevWalkListOptions) -> Vec<ObjectId> {
    let repo = open_grit_repo(wt);
    let opts = FollowWalkOptions {
        walk,
        ..FollowWalkOptions::default()
    };
    let walked = repo
        .rev_walk()
        .follow(&["HEAD"], path, &opts)
        .expect("follow");
    walked.map(|c| c.oid).collect()
}

fn assert_follow_matches_git(
    wt: &Path,
    path: &str,
    walk: RevWalkListOptions,
    git_until: Option<&str>,
) {
    let git_list = git_follow_hashes(wt, path, walk.limit, git_until);
    let grit_list = grit_follow_hashes(wt, path, walk.clone());
    assert_eq!(
        grit_list, git_list,
        "follow mismatch for {path:?} walk={walk:?}"
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
    assert_follow_matches_git(&worktree, "b.txt", RevWalkListOptions::default(), None);
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
    std::fs::write(worktree.join("f.txt"), "two-and-half\n").unwrap();
    git(&worktree, &["add", "f.txt"]);
    git(&worktree, &["mv", "f.txt", "g.txt"]);
    git(&worktree, &["commit", "-m", "rename"]);
    std::fs::write(worktree.join("g.txt"), "three\n").unwrap();
    git(&worktree, &["add", "g.txt"]);
    git(&worktree, &["commit", "-m", "c3"]);
    assert_follow_matches_git(&worktree, "g.txt", RevWalkListOptions::default(), None);
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
    assert_follow_matches_git(&worktree, "c.txt", RevWalkListOptions::default(), None);
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
    assert_follow_matches_git(&worktree, "g.txt", RevWalkListOptions::default(), None);
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
    assert_follow_matches_git(
        &worktree,
        "b.txt",
        RevWalkListOptions {
            limit: Some(3),
            ..RevWalkListOptions::default()
        },
        None,
    );
}

#[test]
fn follow_until_with_newer_head_matches_git() {
    let repo = HistoryRepo::init();
    let worktree = repo.worktree();
    git(worktree, &["config", "user.email", "t@e"]);
    git(worktree, &["config", "user.name", "T"]);
    std::fs::write(worktree.join("f.txt"), "v2016\n").unwrap();
    git(worktree, &["add", "f.txt"]);
    let c2016 = commit_labeled_with_dates(
        worktree,
        "c2016",
        &[],
        "2016-01-01T00:00:00",
        "2016-01-01T00:00:00",
    );
    git(
        worktree,
        &["update-ref", "refs/heads/main", &c2016.to_hex()],
    );
    std::fs::write(worktree.join("f.txt"), "v2018\n").unwrap();
    git(worktree, &["add", "f.txt"]);
    let c2018 = commit_labeled_with_dates(
        worktree,
        "c2018",
        &[c2016],
        "2018-01-01T00:00:00",
        "2018-01-01T00:00:00",
    );
    git(
        worktree,
        &["update-ref", "refs/heads/main", &c2018.to_hex()],
    );
    std::fs::write(worktree.join("f.txt"), "v2020\n").unwrap();
    git(worktree, &["add", "f.txt"]);
    let c2020 = commit_labeled_with_dates(
        worktree,
        "c2020",
        &[c2018],
        "2020-01-01T00:00:00",
        "2020-01-01T00:00:00",
    );
    git(
        worktree,
        &["update-ref", "refs/heads/main", &c2020.to_hex()],
    );
    let until =
        grit_lib::revwalk::committer_cutoff_from_date("2019-06-01 00:00:00 +0000", 1_700_000_000)
            .expect("until");
    assert_follow_matches_git(
        worktree,
        "f.txt",
        RevWalkListOptions {
            until: Some(until),
            ..RevWalkListOptions::default()
        },
        Some("2019-06-01"),
    );
}

#[test]
fn follow_unrelated_dash_path_not_followed() {
    let repo = HistoryRepo::init();
    let worktree = repo.worktree();
    git(worktree, &["config", "user.email", "t@e"]);
    git(worktree, &["config", "user.name", "T"]);
    std::fs::create_dir(worktree.join("dir")).unwrap();
    let mut long = String::new();
    for i in 0..200 {
        long.push_str(&format!("line {i}\n"));
    }
    std::fs::write(worktree.join("dir-file"), &long).unwrap();
    git(worktree, &["add", "dir-file"]);
    git(worktree, &["commit", "-m", "dash"]);
    std::fs::write(worktree.join("dir").join("file"), "other\n").unwrap();
    git(worktree, &["add", "dir/file"]);
    git(worktree, &["commit", "-m", "nested"]);
    assert_follow_matches_git(&worktree, "dir/file", RevWalkListOptions::default(), None);
}

#[test]
fn follow_git_git_builtin_add_c_smoke() {
    let wt = Path::new("/tmp/git-wt");
    if !wt.is_dir() {
        return;
    }
    if git_follow_hashes(wt, "builtin/add.c", None, None).is_empty() {
        return;
    }
    assert_follow_matches_git(wt, "builtin/add.c", RevWalkListOptions::default(), None);
}

/// Side-branch predecessor edits sandwiched between two tip-path commits (git.git 6b3020a shape).
#[test]
fn follow_sandwiched_predecessor_path_matches_git() {
    let repo = HistoryRepo::init();
    let worktree = repo.worktree();
    git(worktree, &["config", "user.email", "t@e"]);
    git(worktree, &["config", "user.name", "T"]);
    std::fs::create_dir_all(worktree.join("dir")).unwrap();
    std::fs::write(worktree.join("dir/sub.txt"), "v1\n").unwrap();
    git(worktree, &["add", "dir/sub.txt"]);
    git(worktree, &["commit", "-m", "tip-path"]);
    git(worktree, &["checkout", "-q", "-b", "side"]);
    std::fs::write(worktree.join("dir-sub.txt"), "side\n").unwrap();
    git(worktree, &["add", "dir-sub.txt"]);
    git(worktree, &["commit", "-m", "predecessor-side"]);
    git(worktree, &["checkout", "-q", "main"]);
    std::fs::write(worktree.join("dir/sub.txt"), "v2\n").unwrap();
    git(worktree, &["add", "dir/sub.txt"]);
    git(worktree, &["commit", "-m", "tip-path-2"]);
    assert_follow_matches_git(
        &worktree,
        "dir/sub.txt",
        RevWalkListOptions::default(),
        None,
    );
}

#[test]
fn follow_resolved_walk_pathspec_matches_git() {
    let repo = HistoryRepo::init();
    let worktree = repo.worktree();
    git(worktree, &["config", "user.email", "t@e"]);
    git(worktree, &["config", "user.name", "T"]);
    std::fs::create_dir_all(worktree.join("sub")).unwrap();
    std::fs::write(worktree.join("sub").join("f.txt"), "x\n").unwrap();
    git(worktree, &["add", "sub/f.txt"]);
    git(worktree, &["commit", "-m", "add"]);
    std::fs::write(worktree.join("sub").join("f.txt"), "y\n").unwrap();
    git(worktree, &["add", "sub/f.txt"]);
    git(worktree, &["commit", "-m", "edit"]);
    let git_list = git_follow_hashes(worktree, "sub/f.txt", None, None);
    let grit = open_grit_repo(worktree);
    let walked = grit
        .rev_walk()
        .follow(
            &["HEAD"],
            "sub/f.txt",
            &FollowWalkOptions {
                walk: RevWalkListOptions {
                    paths: vec!["sub/f.txt".into()],
                    ..RevWalkListOptions::default()
                },
                ..FollowWalkOptions::default()
            },
        )
        .expect("follow");
    let grit_list: Vec<ObjectId> = walked.map(|c| c.oid).collect();
    assert_eq!(grit_list, git_list);
}

#[test]
fn follow_rejects_multiple_paths() {
    let repo = HistoryRepo::init();
    let worktree = repo.worktree();
    git(worktree, &["config", "user.email", "t@e"]);
    git(worktree, &["config", "user.name", "T"]);
    std::fs::write(worktree.join("a.txt"), "x\n").unwrap();
    git(worktree, &["add", "a.txt"]);
    git(worktree, &["commit", "-m", "init"]);
    let grit = Repository::discover(Some(worktree), &RepositoryOptions::empty()).unwrap();
    match grit.rev_walk().follow(
        &["HEAD"],
        "a.txt",
        &FollowWalkOptions {
            walk: RevWalkListOptions {
                paths: vec!["a.txt".into(), "b.txt".into()],
                ..Default::default()
            },
            ..Default::default()
        },
    ) {
        Ok(_) => panic!("expected follow to reject multiple paths"),
        Err(err) => assert!(err.to_string().contains("one path"), "unexpected: {err}"),
    }
}
