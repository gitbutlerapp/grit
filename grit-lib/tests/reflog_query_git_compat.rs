//! Reflog query filters vs system `git reflog` / `git log -g --grep-reflog`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::process::Command;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::refs::store::RefName;
use grit_lib::repo::Repository;
use grit_lib::repo_handles::{MessageMatcher, ReflogQuery};
use grit_lib::revwalk::committer_cutoff_from_date;

use support::{git, git_interop_available, git_ok, Backend, TestRepo};

fn grit_indices(worktree: &std::path::Path, query: &ReflogQuery) -> Vec<usize> {
    let repo = Repository::discover(
        Some(worktree),
        &RepositoryOptions::empty().with_work_tree(worktree),
    )
    .expect("open");
    let name = RefName::try_from("HEAD").expect("HEAD");
    let page = repo.reflogs().query(&name, query).expect("query");
    page.entries.into_iter().map(|e| e.index).collect()
}

fn git_reflog_indices(dir: &std::path::Path, extra: &[&str]) -> Vec<usize> {
    let mut args = vec!["reflog", "HEAD"];
    args.extend(extra);
    let out = git(dir, &args);
    parse_git_reflog_indices(&out)
}

fn git_grep_reflog_indices(dir: &std::path::Path, pattern: &str, limit: usize) -> Vec<usize> {
    let out = git(
        dir,
        &[
            "log",
            "-g",
            &format!("--grep-reflog={pattern}"),
            "-n",
            &limit.to_string(),
            "--format=%gd",
        ],
    );
    out.lines()
        .filter_map(|line| parse_git_reflog_index(line.trim()))
        .collect()
}

fn parse_git_reflog_indices(text: &str) -> Vec<usize> {
    text.lines()
        .filter_map(parse_git_reflog_index_from_line)
        .collect()
}

fn parse_git_reflog_index_from_line(line: &str) -> Option<usize> {
    let start = line.find("@{")?;
    let after = &line[start + 2..];
    let end = after.find('}')?;
    after[..end].parse().ok()
}

fn parse_git_reflog_index(selector: &str) -> Option<usize> {
    parse_git_reflog_index_from_line(selector)
}

fn seed_reflog_repo() -> TestRepo {
    let repo = support::files_repo();
    for i in 0..12 {
        std::fs::write(repo.worktree().join("f"), format!("v{i}")).expect("write");
        git_ok(repo.worktree(), &["add", "f"]);
        git_ok(repo.worktree(), &["commit", "-m", &format!("msg{i}")]);
    }
    repo
}

#[test]
fn reflog_query_limit_matches_git() {
    if !git_interop_available(Backend::Files) {
        eprintln!("SKIP: git not available");
        return;
    }
    let repo = seed_reflog_repo();
    let dir = repo.worktree();
    for n in [0usize, 1, 3, 100] {
        let git_idx = git_reflog_indices(dir, &["--max-count", &n.to_string()]);
        let grit_idx = grit_indices(
            repo.worktree(),
            &ReflogQuery {
                limit: Some(n),
                ..ReflogQuery::default()
            },
        );
        assert_eq!(git_idx, grit_idx, "-n {n}");
    }
}

#[test]
fn reflog_query_since_until_matches_git() {
    if !git_interop_available(Backend::Files) {
        eprintln!("SKIP: git not available");
        return;
    }
    let repo = seed_reflog_repo_with_spread_timestamps();
    let dir = repo.worktree();
    let now = 1_800_000_000_i64;
    let since = committer_cutoff_from_date("@1700000000", now).expect("since");
    let until = committer_cutoff_from_date("@1750000000", now).expect("until");
    for (git_args, query) in [
        (
            vec!["--since", "@1700000000"],
            ReflogQuery {
                since: Some(since),
                ..ReflogQuery::default()
            },
        ),
        (
            vec!["--until", "@1750000000"],
            ReflogQuery {
                until: Some(until),
                ..ReflogQuery::default()
            },
        ),
    ] {
        let git_idx = git_reflog_indices(dir, &git_args);
        let grit_idx = grit_indices(repo.worktree(), &query);
        assert!(
            !git_idx.is_empty() && git_idx.len() < 20,
            "expected a proper subset, git={git_idx:?}"
        );
        assert_eq!(git_idx, grit_idx, "args={git_args:?}");
    }
}

fn seed_reflog_repo_with_spread_timestamps() -> TestRepo {
    use grit_lib::objects::ObjectId;
    use grit_lib::refs::append_reflog;

    let repo = support::files_repo();
    let tip = support::git_empty_commit_oid(repo.worktree());
    let git_dir_path = repo.git_dir();
    let git_dir = git_dir_path.as_path();
    let zero = ObjectId::zero();
    let oid = tip;
    let stamps = [
        1_600_000_000_i64,
        1_700_000_000,
        1_750_000_000,
        1_800_000_000,
        1_850_000_000,
    ];
    for (i, ts) in stamps.iter().enumerate() {
        append_reflog(
            git_dir,
            "HEAD",
            &zero,
            &oid,
            &format!("T <t@example.com> {ts} +0000"),
            &format!("commit: spread{i}"),
            true,
        )
        .expect("append");
    }
    repo
}

#[test]
fn reflog_query_grep_matches_git_log_g() {
    if !git_interop_available(Backend::Files) {
        eprintln!("SKIP: git not available");
        return;
    }
    let repo = seed_reflog_repo();
    let dir = repo.worktree();
    for pattern in ["commit", "msg5"] {
        for n in [5usize, 20] {
            let git_idx = git_grep_reflog_indices(dir, pattern, n);
            let grit_idx = grit_indices(
                repo.worktree(),
                &ReflogQuery {
                    limit: Some(n),
                    grep: Some(MessageMatcher::new(pattern)),
                    ..ReflogQuery::default()
                },
            );
            assert_eq!(git_idx, grit_idx, "grep={pattern} n={n}");
        }
    }
}
