//! Merge blame with repeated lines must match `git blame` (issue #2574).

use grit_lib::progress::{CancelToken, NullProgress};

use std::path::Path;
use std::process::Command;

use grit_lib::blame::{compute_blame, BlameDiffAlgorithm, BlameOptions};
use grit_lib::config::environment::RepositoryOptions;
use grit_lib::diff::WhitespaceMode;
use grit_lib::objects::ObjectId;
use grit_lib::parallel::ThreadPolicy;
use grit_lib::repo::Repository;
use std::collections::{HashMap, HashSet};

fn git_env(cmd: &mut Command) {
    cmd.env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Blame Test")
        .env("GIT_AUTHOR_EMAIL", "blame@example.com")
        .env("GIT_COMMITTER_NAME", "Blame Test")
        .env("GIT_COMMITTER_EMAIL", "blame@example.com");
}

fn git_run(dir: &Path, args: &[&str]) {
    let mut cmd = Command::new("git");
    cmd.current_dir(dir).args(args);
    git_env(&mut cmd);
    let out = cmd.output().expect("spawn git");
    assert!(
        out.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
}

fn git_output(dir: &Path, args: &[&str]) -> String {
    let mut cmd = Command::new("git");
    cmd.current_dir(dir).args(args);
    git_env(&mut cmd);
    let out = cmd.output().expect("spawn git");
    assert!(out.status.success());
    String::from_utf8_lossy(&out.stdout).to_string()
}

fn git_commit(dir: &Path, date: &str, msg: &str) {
    let mut cmd = Command::new("git");
    cmd.current_dir(dir)
        .args(["commit", "-qm", msg])
        .env("GIT_AUTHOR_DATE", date)
        .env("GIT_COMMITTER_DATE", date);
    git_env(&mut cmd);
    let out = cmd.output().expect("commit");
    assert!(
        out.status.success(),
        "commit failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn assert_blame_matches_git(dir: &Path, file: &str) {
    let out = git_output(dir, &["blame", "--line-porcelain", file]);
    let mut git_lines: Vec<(usize, ObjectId)> = Vec::new();
    let mut commit: Option<ObjectId> = None;
    let mut final_line = 0usize;
    for raw in out.lines() {
        if raw.starts_with('\t') {
            if let Some(c) = commit.take() {
                git_lines.push((final_line, c));
            }
            continue;
        }
        let mut parts = raw.split_whitespace();
        let Some(sha) = parts.next() else {
            continue;
        };
        if sha.len() != 40 {
            continue;
        }
        let Ok(c) = sha.parse::<ObjectId>() else {
            continue;
        };
        let Some(fl) = parts.nth(1).and_then(|s| s.parse().ok()) else {
            continue;
        };
        commit = Some(c);
        final_line = fl;
    }
    git_lines.sort_by_key(|(l, _)| *l);

    let repo = Repository::discover(Some(dir), &RepositoryOptions::empty()).expect("open");
    let grit = repo
        .diff()
        .blame(
            file,
            &BlameOptions::default(),
            &mut NullProgress,
            &CancelToken::never(),
        )
        .expect("blame");
    assert_eq!(git_lines.len(), grit.lines.len(), "line count");
    for ((fl, git_oid), line) in git_lines.iter().zip(grit.lines.iter()) {
        assert_eq!(*fl, line.final_line, "final line");
        assert_eq!(*git_oid, line.commit, "commit at line {}", fl);
    }
}

/// Repeated literal `"\n"` lines between independent edits on merging branches.
#[test]
fn blame_merge_repeated_line_coalesced_handoff_matches_git() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git_run(root, &["init", "-q", "-b", "main"]);
    git_run(root, &["config", "user.name", "Blame Test"]);
    git_run(root, &["config", "user.email", "blame@example.com"]);

    std::fs::write(
        root.join("commit.c"),
        "before\n\"\n\"\nmiddle\n\"\n\"\nafter\n",
    )
    .unwrap();
    git_run(root, &["add", "commit.c"]);
    git_commit(root, "1600000100 +0000", "base");

    git_run(root, &["checkout", "-qb", "side"]);
    std::fs::write(
        root.join("commit.c"),
        "BEFORE\n\"\n\"\nmiddle\n\"\n\"\nafter\n",
    )
    .unwrap();
    git_run(root, &["add", "commit.c"]);
    git_commit(root, "1600000200 +0000", "Teach cherry pick");

    git_run(root, &["checkout", "main"]);
    std::fs::write(
        root.join("commit.c"),
        "before\n\"\n\"\nmiddle\n\"\n\"\nAFTER\n",
    )
    .unwrap();
    git_run(root, &["add", "commit.c"]);
    git_commit(root, "1600000250 +0000", "tweak advice");

    git_run(root, &["merge", "--no-ff", "side", "-m", "merge branches"]);
    assert_blame_matches_git(root, "commit.c");
}

/// Whole-file merge blame must agree with `-L` on a repeated blank line (issue #2574).
#[test]
fn blame_merge_whole_file_matches_line_range_on_padded_blank() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git_run(root, &["init", "-q", "-b", "main"]);
    git_run(root, &["config", "user.name", "Blame Test"]);
    git_run(root, &["config", "user.email", "blame@example.com"]);

    let mut body = String::new();
    for i in 1..=120 {
        body.push_str(&format!("head-{i}\n"));
    }
    body.push('\n');
    for i in 1..=120 {
        body.push_str(&format!("tail-{i}\n"));
    }
    std::fs::write(root.join("wide.c"), &body).unwrap();
    git_run(root, &["add", "wide.c"]);
    git_commit(root, "1600000100 +0000", "base wide");

    git_run(root, &["checkout", "-qb", "side"]);
    let mut side = body.clone();
    side = side.replacen("head-10\n", "HEAD-10\n", 1);
    std::fs::write(root.join("wide.c"), side).unwrap();
    git_run(root, &["add", "wide.c"]);
    git_commit(root, "1600000200 +0000", "side edit");

    git_run(root, &["checkout", "main"]);
    let mut main = body.clone();
    main = main.replacen("tail-10\n", "TAIL-10\n", 1);
    std::fs::write(root.join("wide.c"), main).unwrap();
    git_run(root, &["add", "wide.c"]);
    git_commit(root, "1600000250 +0000", "main edit");
    git_run(root, &["merge", "--no-ff", "side", "-m", "merge padded"]);

    assert_blame_matches_git(root, "wide.c");

    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let head = git_output(root, &["rev-parse", "HEAD"]).trim().to_string();
    let head_oid = head.parse::<ObjectId>().expect("head");
    let policy = ThreadPolicy::default();
    let ignore_revs = HashSet::new();
    let grafts = HashMap::new();
    let blank_line = 121usize;
    let whole = compute_blame(
        repo.odb(),
        head_oid,
        "wide.c",
        &ignore_revs,
        BlameDiffAlgorithm::Myers,
        None,
        false,
        0,
        false,
        false,
        &grafts,
        false,
        None,
        WhitespaceMode::default(),
        &policy,
    )
    .expect("whole blame");
    let ranged = compute_blame(
        repo.odb(),
        head_oid,
        "wide.c",
        &ignore_revs,
        BlameDiffAlgorithm::Myers,
        None,
        false,
        0,
        false,
        false,
        &grafts,
        false,
        Some(blank_line..=blank_line),
        WhitespaceMode::default(),
        &policy,
    )
    .expect("ranged blame");
    let whole_line = whole
        .iter()
        .find(|l| l.final_lineno == blank_line)
        .expect("whole line");
    let range_line = ranged
        .iter()
        .find(|l| l.final_lineno == blank_line)
        .expect("range line");
    assert_eq!(
        whole_line.oid, range_line.oid,
        "whole-file and ranged blame must match on repeated blank at merge"
    );
}
