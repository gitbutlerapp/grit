//! Branch inventory vs `git for-each-ref` (upstream, track, worktree, subject).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::refs::branch_tracking::{list_branches, BranchDetail, BranchListQuery, BranchScope};
use grit_lib::refs::listing::RefQuery;
use grit_lib::repo::Repository;
use tempfile::TempDir;

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@e.com")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@e.com")
        .status()
        .expect("git");
    assert!(status.success(), "git {args:?}");
}

fn commit(dir: &Path, file: &str, msg: &str) {
    std::fs::write(dir.join(file), msg.as_bytes()).expect("write");
    git(dir, &["add", file]);
    git(dir, &["commit", "-qm", msg]);
}

struct GitBranchRow {
    oid: String,
    upstream: String,
    track: String,
    worktree: String,
    subject: String,
    committer_unix: String,
}

fn git_branch_rows(dir: &Path, prefix: &str) -> BTreeMap<String, GitBranchRow> {
    let out = Command::new("git")
        .current_dir(dir)
        .args([
            "for-each-ref",
            "--format=%(refname) %(objectname) %(upstream) %(upstream:track) %(worktreepath) %(contents:subject) %(committerdate:unix)",
            prefix,
        ])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("for-each-ref");
    assert!(out.status.success());
    let mut map = BTreeMap::new();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        if line.trim().is_empty() {
            continue;
        }
        let mut parts = line.splitn(7, ' ');
        let full_ref = parts.next().unwrap().to_owned();
        let oid = parts.next().unwrap().to_owned();
        let upstream = parts.next().unwrap_or("").to_owned();
        let track = parts.next().unwrap_or("").to_owned();
        let worktree = parts.next().unwrap_or("").to_owned();
        let subject = parts.next().unwrap_or("").to_owned();
        let committer_unix = parts.next().unwrap_or("").to_owned();
        map.insert(
            full_ref.clone(),
            GitBranchRow {
                oid,
                upstream,
                track,
                worktree,
                subject,
                committer_unix,
            },
        );
    }
    map
}

fn parse_track(track: &str) -> (Option<usize>, Option<usize>, bool) {
    if track.is_empty() {
        return (None, None, false);
    }
    if track.contains("gone") {
        return (None, None, true);
    }
    let mut ahead = None;
    let mut behind = None;
    for token in track.trim_matches(|c| c == '[' || c == ']').split(',') {
        let token = token.trim();
        if let Some(n) = token.strip_prefix("ahead ") {
            ahead = n.trim().parse().ok();
        } else if let Some(n) = token.strip_prefix("behind ") {
            behind = n.trim().parse().ok();
        }
    }
    (ahead, behind, false)
}

fn open(dir: &Path) -> Repository {
    Repository::discover(Some(dir), &RepositoryOptions::empty()).expect("open")
}

fn build_fixture() -> TempDir {
    let tmp = TempDir::new().expect("tempdir");
    let origin = tmp.path().join("origin");
    std::fs::create_dir_all(&origin).expect("mkdir");
    git(&origin, &["init", "-q", "-b", "main"]);
    commit(&origin, "a", "origin-main");
    git(tmp.path(), &["clone", "-q", "origin", "clone"]);
    let clone = tmp.path().join("clone");
    git(&clone, &["fetch", "-q", "origin"]);

    git(&clone, &["branch", "-q", "tracked"]);
    commit(&clone, "b", "local-ahead");
    git(
        &clone,
        &[
            "branch",
            "--set-upstream-to=refs/remotes/origin/main",
            "tracked",
        ],
    );

    git(&clone, &["branch", "-q", "untracked"]);
    commit(&clone, "c", "no-upstream");

    git(&clone, &["branch", "-q", "gone-up"]);
    git(
        &clone,
        &[
            "branch",
            "--set-upstream-to=refs/remotes/origin/main",
            "gone-up",
        ],
    );
    git(
        &clone,
        &["update-ref", "refs/remotes/origin/keep", "refs/heads/main"],
    );
    git(&clone, &["update-ref", "-d", "refs/remotes/origin/main"]);

    git(&clone, &["checkout", "-q", "main"]);
    git(&clone, &["worktree", "add", "-q", "../wt", "untracked"]);
    let _wt = tmp.path().join("wt");

    tmp
}

fn assert_scope_matches_git(dir: &Path, scope: BranchScope, prefix: &str) {
    let repo = open(dir);
    let query = BranchListQuery {
        scope,
        refs: RefQuery::default(),
    };
    let rows = list_branches(&repo, &query, BranchDetail::Full).expect("list");
    let git_rows = git_branch_rows(dir, prefix);

    assert_eq!(rows.len(), git_rows.len(), "row count");
    for row in rows {
        let git_row = git_rows
            .get(&row.full_ref)
            .unwrap_or_else(|| panic!("missing git row for {}", row.full_ref));
        assert_eq!(row.oid.to_hex(), git_row.oid);
        assert_eq!(
            row.subject.as_deref().unwrap_or(""),
            git_row.subject.as_str()
        );

        if let Some(unix) = git_row.committer_unix.parse::<i64>().ok() {
            assert_eq!(
                row.committer_time.map(|t| t.unix_timestamp()),
                Some(unix),
                "{} committer",
                row.full_ref
            );
        }

        if row.is_remote {
            assert!(row.upstream.is_none());
            continue;
        }

        let up = row.upstream.as_ref();
        if git_row.upstream.is_empty() {
            assert!(up.is_none(), "expected no upstream for {}", row.full_ref);
        } else {
            let up = up.expect("upstream");
            assert_eq!(up.full_ref, git_row.upstream);
            let (git_ahead, git_behind, git_gone) = parse_track(&git_row.track);
            assert_eq!(up.gone, git_gone || git_row.track.contains("gone"));
            if !up.gone {
                assert_eq!(up.ahead, git_ahead, "ahead {}", row.full_ref);
                assert_eq!(up.behind, git_behind, "behind {}", row.full_ref);
            }
        }

        let grit_wt = row
            .worktree
            .as_ref()
            .map(|p| p.canonicalize().unwrap_or_else(|_| p.clone()));
        let git_wt = if git_row.worktree.is_empty() {
            None
        } else {
            Some(
                PathBuf::from(&git_row.worktree)
                    .canonicalize()
                    .unwrap_or_else(|_| PathBuf::from(&git_row.worktree)),
            )
        };
        assert_eq!(grit_wt, git_wt, "worktree {}", row.full_ref);
    }
}

#[test]
fn branch_inventory_local_matches_git_for_each_ref() {
    let tmp = build_fixture();
    let clone = tmp.path().join("clone");
    assert_scope_matches_git(&clone, BranchScope::Local, "refs/heads/");
}

#[test]
fn branch_inventory_remote_matches_git_for_each_ref() {
    let tmp = build_fixture();
    let clone = tmp.path().join("clone");
    assert_scope_matches_git(&clone, BranchScope::Remote, "refs/remotes/");
}
