//! `RevWalk::walk` revision specs vs system `git rev-list` (issue #1879).
//!
//! `walk` takes exactly one revision or range. Symmetric ranges (`A...B`) must list the
//! symmetric difference like `git rev-list A...B` and [`RevWalk::commits`]; inputs that only
//! the token-based API understands (options, `--`, several tokens) fail with a typed error.
//! Symmetric ranges list commits in Git's exact order and only read history down to the
//! merge-base frontier (issue #2109).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use grit_lib::config::environment::{Environment, RepositoryOptions};
use grit_lib::error::Error;
use grit_lib::repo::Repository;
use grit_lib::revwalk::rev_list::{rev_list, RevListOptions};
use grit_lib::revwalk::rev_list_error::RevListError;
use grit_lib::revwalk::{parse_rev_walk_tokens, RevWalkListOptions};
use grit_test_support::{git, git_cmd, unique_tmp, write_file};

/// The issue's scratch repository: three base commits, `topic` with two commits, `main` with
/// three more on top of the shared base.
fn diverged_repo() -> PathBuf {
    let wt = unique_tmp("revwalk-walk-specs", "diverged");
    git(&wt, &["init", "-q", "-b", "main", "."]);
    let commit = |file: &str, body: &str, msg: &str| {
        write_file(&wt, file, body);
        git(&wt, &["add", file]);
        git(&wt, &["commit", "-qm", msg]);
    };
    for i in 1..=3 {
        commit("f", &format!("{i}\n"), &format!("base{i}"));
    }
    git(&wt, &["switch", "-qc", "topic"]);
    for i in 1..=2 {
        commit("t", &format!("t{i}\n"), &format!("topic{i}"));
    }
    git(&wt, &["switch", "-q", "main"]);
    for i in 1..=3 {
        commit("m", &format!("m{i}\n"), &format!("main{i}"));
    }
    wt
}

fn open(wt: &Path) -> Repository {
    let mut env = Environment::empty();
    env.cwd = wt.to_path_buf();
    env.pwd = Some(wt.to_string_lossy().into_owned());
    Repository::discover(Some(wt), &RepositoryOptions::with_environment(env)).expect("discover")
}

fn git_rev_list(wt: &Path, spec: &str) -> Vec<String> {
    git(wt, &["rev-list", spec])
        .lines()
        .map(str::to_owned)
        .collect()
}

fn walk_hex(repo: &Repository, spec: &str) -> Vec<String> {
    repo.rev_walk()
        .walk(spec, &RevListOptions::default())
        .unwrap_or_else(|e| panic!("walk({spec}): {e}"))
        .commits
        .iter()
        .map(|oid| oid.to_hex())
        .collect()
}

fn commits_hex(repo: &Repository, spec: &str) -> Vec<String> {
    repo.rev_walk()
        .commits(&[spec], &RevWalkListOptions::default())
        .unwrap_or_else(|e| panic!("commits([{spec}]): {e}"))
        .iter()
        .map(|oid| oid.to_hex())
        .collect()
}

fn as_set(oids: &[String]) -> BTreeSet<&str> {
    oids.iter().map(String::as_str).collect()
}

#[test]
fn walk_symmetric_and_two_dot_ranges_match_git_rev_list() {
    let wt = diverged_repo();
    let repo = open(&wt);
    for (spec, expected_len) in [
        ("main...topic", 5),
        ("topic...main", 5),
        ("main...", 0),
        ("...main", 0),
        ("topic...", 5),
        ("...topic", 5),
        ("main..topic", 2),
        ("topic..main", 3),
        ("main", 6),
        ("^topic", 0),
    ] {
        let expected = git_rev_list(&wt, spec);
        let walked = walk_hex(&repo, spec);
        assert_eq!(walked.len(), expected_len, "walk({spec}) count");
        assert_eq!(walked.len(), expected.len(), "walk({spec}) vs git count");
        assert_eq!(
            as_set(&walked),
            as_set(&expected),
            "walk({spec}) vs git set"
        );
        assert_eq!(walked, commits_hex(&repo, spec), "walk({spec}) vs commits");
    }
}

#[test]
fn walk_symmetric_range_from_detached_side_matches_git() {
    let wt = diverged_repo();
    git(&wt, &["switch", "-q", "topic"]);
    let repo = open(&wt);
    for spec in ["main...", "...main", "main...HEAD", "HEAD...main"] {
        let expected = git_rev_list(&wt, spec);
        assert_eq!(expected.len(), 5, "git rev-list {spec}");
        let walked = walk_hex(&repo, spec);
        assert_eq!(
            as_set(&walked),
            as_set(&expected),
            "walk({spec}) vs git set"
        );
        assert_eq!(walked, commits_hex(&repo, spec), "walk({spec}) vs commits");
    }
}

#[test]
fn walk_symmetric_range_honours_max_count_and_left_right() {
    let wt = diverged_repo();
    let repo = open(&wt);
    let limited = repo
        .rev_walk()
        .walk(
            "main...topic",
            &RevListOptions {
                max_count: Some(2),
                ..RevListOptions::default()
            },
        )
        .expect("walk -n 2");
    let expected: Vec<String> = git(&wt, &["rev-list", "-n", "2", "main...topic"])
        .lines()
        .map(str::to_owned)
        .collect();
    let got: Vec<String> = limited.commits.iter().map(|o| o.to_hex()).collect();
    assert_eq!(got, expected);

    let left_right = repo
        .rev_walk()
        .walk(
            "main...topic",
            &RevListOptions {
                left_right: true,
                ..RevListOptions::default()
            },
        )
        .expect("walk --left-right");
    let git_marks: BTreeSet<String> = git(&wt, &["rev-list", "--left-right", "main...topic"])
        .lines()
        .map(str::to_owned)
        .collect();
    let grit_marks: BTreeSet<String> = left_right
        .commits
        .iter()
        .map(|oid| {
            let mark = if left_right.left_right_map.get(oid).copied().unwrap_or(false) {
                '<'
            } else {
                '>'
            };
            format!("{mark}{}", oid.to_hex())
        })
        .collect();
    assert_eq!(grit_marks, git_marks);
}

#[test]
fn walk_rejects_specs_that_need_the_token_api() {
    let wt = diverged_repo();
    let repo = open(&wt);
    for spec in [
        "--all",
        "--branches",
        "--",
        "-n",
        "HEAD ^HEAD~2",
        "main\ttopic",
    ] {
        let err = repo
            .rev_walk()
            .walk(spec, &RevListOptions::default())
            .expect_err(spec);
        match err {
            Error::RevList(RevListError::UnsupportedWalkSpec { spec: got }) => {
                assert_eq!(got, spec, "error keeps the full spec");
            }
            other => panic!("walk({spec:?}): expected UnsupportedWalkSpec, got {other:?}"),
        }
    }
}

#[test]
fn walk_accepts_single_specs_containing_spaces() {
    let wt = diverged_repo();
    let repo = open(&wt);
    let search = walk_hex(&repo, ":/topic2");
    assert_eq!(search, git_rev_list(&wt, ":/topic2"));
    let spaced = walk_hex(&repo, "main^{/main 1|main1}");
    assert_eq!(spaced, git_rev_list(&wt, "main^{/main 1|main1}"));
}

/// `diverged_repo` plus two commits on `main`: `fix...up` (adding `dir/a...b`) and `after`.
fn ellipsis_repo() -> PathBuf {
    let wt = diverged_repo();
    std::fs::create_dir_all(wt.join("dir")).expect("mkdir dir");
    write_file(&wt, "dir/a...b", "x\n");
    git(&wt, &["add", "dir"]);
    git(&wt, &["commit", "-qm", "fix...up"]);
    write_file(&wt, "f", "after\n");
    git(&wt, &["commit", "-qam", "after"]);
    wt
}

#[test]
fn walk_keeps_literal_ellipses_in_selectors_and_paths() {
    let wt = ellipsis_repo();
    let repo = open(&wt);
    for (spec, expected_len) in [
        (":/fix...up", 7),
        ("main^{/fix...up}", 7),
        ("^:/fix...up", 0),
        ("topic...main^{/fix...up}", 6),
    ] {
        let expected = git_rev_list(&wt, spec);
        assert_eq!(expected.len(), expected_len, "git rev-list {spec}");
        assert_eq!(walk_hex(&repo, spec), expected, "walk({spec}) vs git");
        assert_eq!(
            commits_hex(&repo, spec),
            expected,
            "commits([{spec}]) vs git"
        );
    }

    let path_spec = "HEAD:dir/a...b";
    let parsed = parse_rev_walk_tokens(&repo, &[path_spec]).expect("parse path spec");
    assert_eq!(parsed.positive, vec![path_spec.to_owned()]);
    assert!(parsed.negative.is_empty() && parsed.symmetric_left.is_none());
    assert_eq!(
        repo.rev_walk()
            .resolve(path_spec)
            .expect("resolve")
            .to_hex(),
        git(&wt, &["rev-parse", path_spec]).trim()
    );

    let direct = rev_list(
        &repo,
        &[":/fix...up".to_owned(), "main^{/fix...up}".to_owned()],
        &[],
        &RevListOptions::default(),
    )
    .expect("selectors with literal dots are single revisions");
    assert_eq!(
        direct
            .commits
            .iter()
            .map(|o| o.to_hex())
            .collect::<Vec<_>>(),
        git_rev_list(&wt, ":/fix...up")
    );
}

#[test]
fn rev_list_rejects_unexpanded_symmetric_range() {
    let wt = diverged_repo();
    let repo = open(&wt);
    let err = rev_list(
        &repo,
        &["main...topic".to_owned()],
        &[],
        &RevListOptions::default(),
    )
    .expect_err("raw A...B must not resolve to its merge base");
    assert!(
        matches!(
            &err,
            Error::RevList(RevListError::UnexpandedSymmetricRange { spec }) if spec == "main...topic"
        ),
        "{err:?}"
    );
}

/// Builds a `git fast-import` stream of commits with explicit committer dates.
#[derive(Default)]
struct ImportStream {
    text: String,
    marks: usize,
}

impl ImportStream {
    /// Append a commit on `branch` and return its mark. `parents` are marks, or an empty
    /// slice for a root commit; `from` names an existing commit instead of a mark.
    fn commit(&mut self, branch: &str, date: i64, parents: &[usize]) -> usize {
        self.commit_from(branch, date, None, parents)
    }

    fn commit_from(
        &mut self,
        branch: &str,
        date: i64,
        from: Option<&str>,
        parents: &[usize],
    ) -> usize {
        self.marks += 1;
        let mark = self.marks;
        let message = format!("c{mark}\n");
        self.text.push_str(&format!(
            "commit refs/heads/{branch}\nmark :{mark}\ncommitter T <t@example.com> {date} +0000\ndata {}\n{message}",
            message.len()
        ));
        if let Some(from) = from {
            self.text.push_str(&format!("from {from}\n"));
        }
        for (i, parent) in parents.iter().enumerate() {
            let verb = if i == 0 && from.is_none() {
                "from"
            } else {
                "merge"
            };
            self.text.push_str(&format!("{verb} :{parent}\n"));
        }
        self.text.push_str(&format!(
            "M 100644 inline f{mark}\ndata 2\n{}\n\n",
            mark % 10
        ));
        mark
    }

    fn import(self, wt: &Path) {
        git_cmd(&["fast-import", "--quiet"])
            .in_dir(wt)
            .stdin(self.text)
            .suc();
    }
}

/// Branches with tied and skewed committer dates, a topic that merged `main` back in, and
/// a criss-cross pair (`x`, `y`) with two merge bases.
fn symmetric_shapes_repo() -> PathBuf {
    let wt = unique_tmp("revwalk-walk-specs", "symmetric-shapes");
    git(&wt, &["init", "-q", "-b", "main", "."]);
    let mut s = ImportStream::default();
    let root = s.commit("main", 100, &[]);
    let b2 = s.commit("main", 200, &[root]);
    let base = s.commit("main", 300, &[b2]);
    let t1 = s.commit("topic", 400, &[base]);
    let t2 = s.commit("topic", 500, &[t1]);
    let m1 = s.commit("main", 400, &[base]);
    let m2 = s.commit("main", 450, &[m1]);
    let m3 = s.commit("main", 500, &[m2]);
    let s1 = s.commit("skew", 350, &[base]);
    let s2 = s.commit("skew", 250, &[s1]);
    s.commit("skew", 600, &[s2]);
    let x1 = s.commit("x", 700, &[m3]);
    let y1 = s.commit("y", 700, &[m3]);
    let x2 = s.commit("x", 800, &[x1, y1]);
    let y2 = s.commit("y", 800, &[y1, x1]);
    s.commit("x", 900, &[x2]);
    s.commit("y", 850, &[y2]);
    let merged = s.commit("topic", 650, &[t2, m2]);
    s.commit("topic", 660, &[merged]);
    s.import(&wt);
    wt
}

fn git_lines(wt: &Path, args: &[&str]) -> Vec<String> {
    git(wt, args).lines().map(str::to_owned).collect()
}

#[test]
fn symmetric_ranges_list_commits_in_git_order() {
    let wt = symmetric_shapes_repo();
    let merge_bases = git_lines(&wt, &["merge-base", "--all", "x", "y"]);
    assert_eq!(merge_bases.len(), 2, "criss-cross has two merge bases");
    assert_symmetric_ranges_match_git(&wt);
    git(&wt, &["commit-graph", "write", "--reachable"]);
    assert_symmetric_ranges_match_git(&wt);
}

fn assert_symmetric_ranges_match_git(wt: &Path) {
    let repo = open(wt);
    for spec in [
        "main...topic",
        "topic...main",
        "main...skew",
        "skew...topic",
        "x...y",
        "y...x",
        "main...x",
        "x...main",
        "main~2...topic",
        "topic...topic",
    ] {
        let expected = git_rev_list(wt, spec);
        assert_eq!(walk_hex(&repo, spec), expected, "walk({spec}) vs git order");
        assert_eq!(
            commits_hex(&repo, spec),
            expected,
            "commits([{spec}]) vs git order"
        );
        let count: usize = git(wt, &["rev-list", "--count", spec])
            .trim()
            .parse()
            .expect("git count");
        assert_eq!(
            repo.rev_walk()
                .count(&[spec], &RevWalkListOptions::default())
                .expect("count"),
            count,
            "count([{spec}]) vs git"
        );
        for limit in [1, 2, 3] {
            let n = limit.to_string();
            let expected = git_lines(wt, &["rev-list", "-n", &n, spec]);
            let listed: Vec<String> = repo
                .rev_walk()
                .commits(
                    &[spec],
                    &RevWalkListOptions {
                        limit: Some(limit),
                        ..RevWalkListOptions::default()
                    },
                )
                .expect("commits -n")
                .iter()
                .map(|oid| oid.to_hex())
                .collect();
            assert_eq!(listed, expected, "commits([{spec}]) -n {limit} vs git");
        }
    }
}

/// A long linear history packed on its own, then a short base chain with `main` and
/// `topic` diverging on top in a second pack. Returns the work tree and the deep pack's
/// files, which the caller deletes so any walk below the merge-base frontier fails.
fn deep_history_with_short_topic() -> (PathBuf, Vec<PathBuf>) {
    let wt = unique_tmp("revwalk-walk-specs", "deep-history");
    git(&wt, &["init", "-q", "-b", "main", "."]);
    let mut deep = ImportStream::default();
    let mut tip = deep.commit("main", 1_000, &[]);
    for i in 1..2_000 {
        tip = deep.commit("main", 1_000 + i, &[tip]);
    }
    deep.import(&wt);
    let pack_dir = wt.join(".git/objects/pack");
    let deep_pack: Vec<PathBuf> = std::fs::read_dir(&pack_dir)
        .expect("read pack dir")
        .map(|entry| entry.expect("pack entry").path())
        .collect();
    assert!(!deep_pack.is_empty(), "fast-import wrote a pack");

    let deep_tip = git(&wt, &["rev-parse", "main"]).trim().to_owned();
    let mut recent = ImportStream::default();
    let mut base = recent.commit_from("main", 10_000, Some(&deep_tip), &[]);
    for i in 1..10 {
        base = recent.commit("main", 10_000 + i, &[base]);
    }
    let mut topic = base;
    for i in 0..4 {
        topic = recent.commit("topic", 20_000 + i, &[topic]);
    }
    let mut main = base;
    for i in 0..3 {
        main = recent.commit("main", 20_000 + i, &[main]);
    }
    recent.import(&wt);
    (wt, deep_pack)
}

#[test]
fn symmetric_range_walk_stops_at_merge_base_frontier() {
    let (wt, deep_pack) = deep_history_with_short_topic();
    let specs = ["main...topic", "topic...main", "main..topic"];
    let expected: Vec<Vec<String>> = specs.iter().map(|spec| git_rev_list(&wt, spec)).collect();
    assert_eq!(expected[0].len(), 7);
    let limited = git_lines(&wt, &["rev-list", "-n", "2", "main...topic"]);
    for file in &deep_pack {
        std::fs::remove_file(file).expect("remove deep pack");
    }

    let repo = open(&wt);
    for (spec, expected) in specs.iter().zip(&expected) {
        assert_eq!(&walk_hex(&repo, spec), expected, "walk({spec})");
        assert_eq!(&commits_hex(&repo, spec), expected, "commits([{spec}])");
        if !spec.contains("...") {
            continue;
        }
        assert_eq!(
            repo.rev_walk()
                .count(&[spec], &RevWalkListOptions::default())
                .unwrap_or_else(|e| panic!("count([{spec}]): {e}")),
            expected.len(),
            "count([{spec}])"
        );
    }
    let listed: Vec<String> = repo
        .rev_walk()
        .commits(
            &["main...topic"],
            &RevWalkListOptions {
                limit: Some(2),
                ..RevWalkListOptions::default()
            },
        )
        .expect("commits -n 2")
        .iter()
        .map(|oid| oid.to_hex())
        .collect();
    assert_eq!(listed, limited);
}
