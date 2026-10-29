//! Path-limited history with `--follow` vs system `git log --follow`.

use crate::support;

use std::path::Path;

use grit_lib::revwalk::RevWalkListOptions;

use support::{git, open_grit_repo, HistoryRepo};

fn grit_follow_commits(wt: &Path, path: &str, limit: Option<usize>) -> Vec<String> {
    let repo = open_grit_repo(wt);
    repo.rev_walk()
        .commits(
            &["HEAD"],
            &RevWalkListOptions {
                paths: vec![path.to_owned()],
                follow: true,
                limit,
                ..RevWalkListOptions::default()
            },
        )
        .expect("grit follow walk")
        .into_iter()
        .map(|oid| oid.to_hex())
        .collect()
}

fn git_follow_commits(wt: &Path, path: &str, limit: Option<usize>) -> Vec<String> {
    let mut args = vec!["log", "--follow", "--pretty=%H"];
    let limit_s;
    if let Some(n) = limit {
        args.push("-n");
        limit_s = n.to_string();
        args.push(limit_s.as_str());
    }
    args.push("--");
    args.push(path);
    git(wt, &args)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_owned)
        .collect()
}

fn init_rename_repro(wt: &Path) {
    git(wt, &["init", "-q"]);
    std::fs::write(wt.join("old.txt"), "a\nb\nc\nd\ne\nf\ng\nh\n").expect("write");
    git(wt, &["add", "old.txt"]);
    git(wt, &["commit", "-m", "add"]);
    std::fs::OpenOptions::new()
        .append(true)
        .open(wt.join("old.txt"))
        .expect("open")
        .write_all(b"more\n")
        .expect("append");
    git(wt, &["add", "old.txt"]);
    git(wt, &["commit", "-m", "edit1"]);
    git(wt, &["mv", "old.txt", "new.txt"]);
    git(wt, &["commit", "-m", "rename"]);
    std::fs::OpenOptions::new()
        .append(true)
        .open(wt.join("new.txt"))
        .expect("open")
        .write_all(b"more\n")
        .expect("append");
    git(wt, &["add", "new.txt"]);
    git(wt, &["commit", "-m", "edit2"]);
}

use std::io::Write;

#[test]
fn follow_rename_repro_lists_pre_rename_commits() {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    init_rename_repro(wt);

    let grit = grit_follow_commits(wt, "new.txt", None);
    let git_oids = git_follow_commits(wt, "new.txt", None);
    assert_eq!(grit, git_oids, "follow history must match git");

    let subjects: Vec<_> = git(wt, &["log", "--follow", "--oneline", "--", "new.txt"])
        .lines()
        .map(str::to_owned)
        .collect();
    assert_eq!(subjects.len(), 4, "expected add through edit2");
    assert!(
        subjects.last().unwrap().contains("add"),
        "oldest commit must be add: {:?}",
        subjects
    );
}

#[test]
fn follow_without_flag_stops_at_rename() {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    init_rename_repro(wt);

    let repo = open_grit_repo(wt);
    let oids = repo
        .rev_walk()
        .commits(
            &["HEAD"],
            &RevWalkListOptions {
                paths: vec!["new.txt".to_owned()],
                ..RevWalkListOptions::default()
            },
        )
        .expect("no-follow walk");
    assert_eq!(oids.len(), 2, "rename and edit2 only without follow");
}

#[test]
fn follow_two_rename_chain_matches_git() {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    git(wt, &["init", "-q"]);
    std::fs::write(wt.join("a.txt"), "v1\n").expect("write");
    git(wt, &["add", "a.txt"]);
    git(wt, &["commit", "-m", "c1"]);
    git(wt, &["mv", "a.txt", "b.txt"]);
    git(wt, &["commit", "-m", "r1"]);
    git(wt, &["mv", "b.txt", "c.txt"]);
    git(wt, &["commit", "-m", "r2"]);

    let grit = grit_follow_commits(wt, "c.txt", None);
    let git_oids = git_follow_commits(wt, "c.txt", None);
    assert_eq!(grit, git_oids);
    assert_eq!(grit.len(), 3);
}

#[test]
fn follow_respects_limit_after_path_switch() {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    init_rename_repro(wt);

    let grit = grit_follow_commits(wt, "new.txt", Some(2));
    let git_oids = git_follow_commits(wt, "new.txt", Some(2));
    assert_eq!(grit, git_oids);
    assert_eq!(grit.len(), 2);
}

#[test]
fn follow_detects_renames_when_diff_renames_disabled() {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    init_rename_repro(wt);
    git(wt, &["config", "diff.renames", "false"]);

    let grit = grit_follow_commits(wt, "new.txt", None);
    let git_oids = git_follow_commits(wt, "new.txt", None);
    assert_eq!(grit, git_oids);
    assert_eq!(grit.len(), 4);
}

fn commit_file(wt: &Path, path: &str, content: &str, msg: &str) {
    std::fs::write(wt.join(path), content).expect("write");
    git(wt, &["add", path]);
    git(wt, &["commit", "-m", msg]);
}

#[test]
fn follow_skips_low_similarity_rename() {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    git(wt, &["init", "-q"]);
    commit_file(wt, "old.txt", "aaaaaaaaaa\n", "add");
    commit_file(wt, "other.txt", "bbbbbbbbbb\n", "noise");
    std::fs::remove_file(wt.join("old.txt")).expect("rm");
    std::fs::write(wt.join("new.txt"), "cccccccccc\n").expect("write new");
    git(wt, &["add", "-A"]);
    git(wt, &["commit", "-m", "unrelated-rename"]);

    let grit = grit_follow_commits(wt, "new.txt", None);
    let git_oids = git_follow_commits(wt, "new.txt", None);
    assert_eq!(grit, git_oids);
    assert_eq!(grit.len(), 1, "dissimilar rename must not extend history");
}
