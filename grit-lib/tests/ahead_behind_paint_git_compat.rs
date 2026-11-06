//! `TipComparison::compute` vs `git rev-list --left-right --count` on diverged,
//! repeatedly merged, and clock-skewed histories.

#[allow(clippy::expect_used, clippy::unwrap_used)]
use std::fmt::Write as _;
use std::io::Write as _;
use std::path::Path;
use std::process::{Command, Stdio};

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::ObjectId;
use grit_lib::refs::branch_tracking::{TargetTipListOptions, TargetTipLists, TipComparison};
use grit_lib::repo::Repository;
use tempfile::TempDir;

fn git(root: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(root)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).expect("utf8")
}

/// `(left-only, right-only)` for `left...right`.
fn git_left_right(root: &Path, left: &str, right: &str) -> (usize, usize) {
    let out = git(
        root,
        &[
            "rev-list",
            "--left-right",
            "--count",
            &format!("{left}...{right}"),
        ],
    );
    let mut parts = out
        .split_whitespace()
        .map(|n| n.parse::<usize>().expect("count"));
    (parts.next().expect("left"), parts.next().expect("right"))
}

fn rev(root: &Path, spec: &str) -> ObjectId {
    git(root, &["rev-parse", spec]).trim().parse().expect("oid")
}

/// Builds a history through `git fast-import` with explicit committer dates.
struct History {
    stream: String,
    next_mark: usize,
}

impl History {
    fn new() -> Self {
        Self {
            stream: String::new(),
            next_mark: 1,
        }
    }

    /// Append a commit to `branch` (`from` sets the first parent, `merges` extra
    /// parents, all as marks) and return its mark.
    fn commit(&mut self, branch: &str, time: i64, from: Option<usize>, merges: &[usize]) -> usize {
        let mark = self.next_mark;
        self.next_mark += 1;
        let msg = format!("c{mark}");
        let s = &mut self.stream;
        writeln!(s, "commit refs/heads/{branch}").unwrap();
        writeln!(s, "mark :{mark}").unwrap();
        writeln!(s, "committer T <t@e.com> {time} +0000").unwrap();
        writeln!(s, "data {}\n{msg}", msg.len()).unwrap();
        if let Some(from) = from {
            writeln!(s, "from :{from}").unwrap();
        }
        for m in merges {
            writeln!(s, "merge :{m}").unwrap();
        }
        writeln!(s, "M 644 inline f{mark}\ndata 1\nx").unwrap();
        mark
    }

    fn import(self, root: &Path) {
        git(root, &["init", "-q", "-b", "main", "."]);
        let mut child = Command::new("git")
            .current_dir(root)
            .args(["fast-import", "--quiet"])
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .stdin(Stdio::piped())
            .spawn()
            .expect("spawn fast-import");
        child
            .stdin
            .take()
            .expect("stdin")
            .write_all(self.stream.as_bytes())
            .expect("write stream");
        assert!(child.wait().expect("fast-import").success());
    }
}

fn assert_matches_git(root: &Path, head: &str, target: &str, retain: usize) {
    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let cmp =
        TipComparison::compute(&repo, rev(root, head), rev(root, target), retain).expect("compute");
    let (ahead, behind) = git_left_right(root, head, target);
    assert_eq!(
        (cmp.ahead, cmp.behind),
        (ahead, behind),
        "{head}...{target}"
    );
    assert_eq!(
        cmp.newest_ahead,
        git_newest(root, head, target, retain),
        "newest ahead of {head}...{target}"
    );

    let lists = TargetTipLists::compute(
        &repo,
        rev(root, head),
        rev(root, target),
        TargetTipListOptions {
            list_limit: retain,
            include_behind: true,
        },
    )
    .expect("tip lists");
    assert_eq!((lists.ahead, lists.behind), (ahead, behind));
    assert_eq!(lists.ahead_oids, cmp.newest_ahead);
    assert_eq!(
        lists.behind_oids,
        git_newest(root, target, head, retain),
        "newest behind of {head}...{target}"
    );
}

/// `git rev-list --max-count=<limit> <from> ^<hide>`.
fn git_newest(root: &Path, from: &str, hide: &str, limit: usize) -> Vec<ObjectId> {
    git(
        root,
        &[
            "rev-list",
            &format!("--max-count={limit}"),
            from,
            &format!("^{hide}"),
        ],
    )
    .lines()
    .map(|l| l.parse().expect("oid"))
    .collect()
}

/// A workspace-style branch that keeps merging `main` in while `main` moves on.
fn criss_cross_history(root: &Path) {
    let mut h = History::new();
    let mut time = 1_000_000;
    let mut main = h.commit("main", time, None, &[]);
    for _ in 0..30 {
        time += 60;
        main = h.commit("main", time, Some(main), &[]);
    }
    let mut topic = main;
    for round in 0..8 {
        for _ in 0..5 {
            time += 60;
            topic = h.commit("topic", time, Some(topic), &[]);
        }
        for _ in 0..12 {
            time += 60;
            main = h.commit("main", time, Some(main), &[]);
        }
        if round < 7 {
            time += 60;
            topic = h.commit("topic", time, Some(topic), &[main]);
        }
    }
    h.import(root);
}

#[test]
fn ahead_behind_matches_git_on_repeatedly_merged_branches() {
    let dir = TempDir::new().expect("tempdir");
    criss_cross_history(dir.path());
    for (head, target) in [
        ("topic", "main"),
        ("main", "topic"),
        ("topic~3", "main~20"),
        ("main~40", "topic"),
        ("topic", "topic"),
    ] {
        assert_matches_git(dir.path(), head, target, 5);
    }
}

#[test]
fn ahead_behind_matches_git_with_commit_graph() {
    let dir = TempDir::new().expect("tempdir");
    criss_cross_history(dir.path());
    git(dir.path(), &["commit-graph", "write", "--reachable"]);
    assert_matches_git(dir.path(), "topic", "main", 5);
    assert_matches_git(dir.path(), "main", "topic~10", 3);
}

/// `main` reaches `base` through a long run of commits dated before `base`.
/// Bounded date-ordered ancestry probes give up inside that run and count
/// `base` as ahead; generation numbers keep the paint exact.
#[test]
fn ahead_behind_is_exact_across_clock_skew_with_commit_graph() {
    let dir = TempDir::new().expect("tempdir");
    let mut h = History::new();
    let root = h.commit("main", 1_000, None, &[]);
    let base = h.commit("topic", 50_000, Some(root), &[]);
    let mut main = base;
    for i in 0..300 {
        main = h.commit("main", 2_000 + i, Some(main), &[]);
    }
    let mut topic = base;
    for i in 0..3 {
        topic = h.commit("topic", 60_000 + i, Some(topic), &[]);
    }
    let _ = (main, topic);
    h.import(dir.path());
    git(dir.path(), &["commit-graph", "write", "--reachable"]);

    assert_matches_git(dir.path(), "topic", "main", 10);
    assert_matches_git(dir.path(), "main", "topic", 2);
}
