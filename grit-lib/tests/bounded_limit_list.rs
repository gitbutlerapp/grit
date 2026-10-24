//! Regression tests for bounded `rev-list` / `grit log` fast paths.

mod support;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::error::Error;
use grit_lib::objects::odb::Odb;
use grit_lib::objects::{parse_commit, ObjectKind};
use grit_lib::repo::{InitOptions, Repository};
use grit_lib::revwalk::rev_list::{rev_list, OrderingMode, RevListOptions};
use grit_lib::revwalk::RevWalkListOptions;
use std::fs;
use std::path::Path;
use tempfile::tempdir;

fn empty_tree(odb: &Odb) -> grit_lib::error::Result<grit_lib::objects::ObjectId> {
    odb.write_loose_materialize(ObjectKind::Tree, b"")
}

fn write_commit(
    odb: &Odb,
    parents: &[grit_lib::objects::ObjectId],
    msg: &str,
    author_time: i64,
    committer_time: i64,
) -> grit_lib::error::Result<grit_lib::objects::ObjectId> {
    let tree = empty_tree(odb)?;
    let mut body = format!("tree {tree}\n");
    for p in parents {
        body.push_str(&format!("parent {p}\n"));
    }
    body.push_str(&format!(
        "author T <t@e.com> {author_time} +0000\ncommitter T <t@e.com> {committer_time} +0000\n\n{msg}\n"
    ));
    odb.write_loose_materialize(ObjectKind::Commit, body.as_bytes())
}

fn open_bare(dir: &Path) -> grit_lib::error::Result<Repository> {
    Repository::init(
        dir,
        &InitOptions::new()
            .with_bare(true)
            .with_initial_branch("main".to_owned())
            .with_ref_storage(grit_lib::refs::ref_storage::RefStorageFormat::Files),
    )?;
    Repository::open(dir, &RepositoryOptions::empty())
}

#[test]
fn bounded_log_on_depth_one_shallow_does_not_emit_missing_parent() {
    let dir = tempdir().expect("tempdir");
    let repo = open_bare(dir.path()).expect("repo");
    let missing: grit_lib::objects::ObjectId = "0000000000000000000000000000000000000001"
        .parse()
        .expect("oid");
    let tip = write_commit(repo.odb(), &[missing], "tip", 100, 100).expect("commit");

    fs::write(repo.git_dir().join("shallow"), format!("{tip}\n")).expect("shallow");

    let opts = RevListOptions {
        max_count: Some(11),
        ..Default::default()
    };
    let result = rev_list(&repo, &[tip.to_hex()], &[], &opts).expect("rev-list");
    assert_eq!(result.commits, vec![tip]);
}

#[test]
fn rev_list_errors_on_missing_parent_without_shallow() {
    let dir = tempdir().expect("tempdir");
    let repo = open_bare(dir.path()).expect("repo");
    let missing: grit_lib::objects::ObjectId = "0000000000000000000000000000000000000001"
        .parse()
        .expect("oid");
    let tip = write_commit(repo.odb(), &[missing], "tip", 100, 100).expect("commit");

    let opts = RevListOptions {
        max_count: Some(5),
        ..Default::default()
    };
    let err = rev_list(&repo, &[tip.to_hex()], &[], &opts).unwrap_err();
    assert!(matches!(err, Error::ObjectNotFound(_)));
}

#[test]
fn author_date_order_with_max_count_uses_author_timestamps() {
    let dir = tempdir().expect("tempdir");
    let repo = open_bare(dir.path()).expect("repo");
    let root = write_commit(repo.odb(), &[], "root", 50, 50).expect("root");
    // `a`: older author, newer committer.
    let a = write_commit(repo.odb(), &[root], "a", 100, 400).expect("a");
    // `b`: newer author, older committer.
    let b = write_commit(repo.odb(), &[root], "b", 300, 200).expect("b");

    let opts = RevListOptions {
        max_count: Some(2),
        ordering: OrderingMode::AuthorDateWalk,
        ..Default::default()
    };
    let result = rev_list(&repo, &[a.to_hex(), b.to_hex()], &[], &opts).expect("rev-list");
    assert_eq!(result.commits, vec![b, a]);
}

#[test]
fn exclude_first_parent_only_with_max_count_matches_git() {
    let dir = tempdir().expect("tempdir");
    let repo = open_bare(dir.path()).expect("repo");
    let root = write_commit(repo.odb(), &[], "root", 10, 10).expect("root");
    let first = write_commit(repo.odb(), &[root], "first", 20, 20).expect("first");
    let second = write_commit(repo.odb(), &[root], "second", 30, 30).expect("second");
    let merge = write_commit(repo.odb(), &[first, second], "merge", 40, 40).expect("merge");

    let opts = RevListOptions {
        max_count: Some(1),
        exclude_first_parent_only: true,
        ..Default::default()
    };
    let result = rev_list(&repo, &[second.to_hex()], &[merge.to_hex()], &opts).expect("rev-list");
    assert_eq!(result.commits, vec![second]);
}

#[test]
fn bounded_path_limit_stops_at_first_matching_commit() {
    let repo = support::HistoryRepo::init();
    let wt = repo.worktree();
    let touch = support::commit_file(wt, "tracked.txt", "t\n", "touch tracked");
    for i in 0..120 {
        let content = format!("n{i}\n");
        let msg = format!("noise {i}");
        support::commit_file(wt, "noise.txt", &content, &msg);
    }

    let grit_repo = Repository::discover(Some(wt), &RepositoryOptions::empty()).expect("discover");
    let opts = RevListOptions {
        max_count: Some(1),
        paths: vec!["tracked.txt".to_owned()],
        ..Default::default()
    };
    let result = rev_list(&grit_repo, &[String::from("HEAD")], &[], &opts).expect("rev-list");
    assert_eq!(result.commits, vec![touch]);

    support::assert_grit_commits_match_git(
        wt,
        &["-n", "1", "HEAD", "--", "tracked.txt"],
        &[String::from("HEAD")],
        &[],
        &opts,
    );
}

#[test]
fn bounded_path_limit_skip_and_max_match_git() {
    let repo = support::HistoryRepo::init();
    let wt = repo.worktree();
    support::commit_file(wt, "tracked.txt", "v0\n", "v0");
    let mut prior = support::commit_file(wt, "tracked.txt", "v1\n", "v1");
    for i in 2..80 {
        let content = format!("v{i}\n");
        let msg = format!("v{i}");
        prior = support::commit_file(wt, "tracked.txt", &content, &msg);
    }
    let _newest = prior;

    let opts = RevListOptions {
        max_count: Some(3),
        skip: 1,
        paths: vec!["tracked.txt".to_owned()],
        ..Default::default()
    };
    support::assert_grit_commits_match_git(
        wt,
        &["-n", "3", "--skip", "1", "HEAD", "--", "tracked.txt"],
        &[String::from("HEAD")],
        &[],
        &opts,
    );
}

#[test]
fn bounded_path_limit_zero_emits_nothing_at_matching_tip() {
    let repo = support::HistoryRepo::init();
    let wt = repo.worktree();
    support::commit_file(wt, "tracked.txt", "v\n", "touch");

    let opts = RevListOptions {
        max_count: Some(0),
        paths: vec!["tracked.txt".to_owned()],
        ..Default::default()
    };
    support::assert_grit_commits_match_git(
        wt,
        &["--max-count=0", "HEAD", "--", "tracked.txt"],
        &[String::from("HEAD")],
        &[],
        &opts,
    );
}

#[test]
fn large_max_count_without_paths_matches_git() {
    let repo = support::HistoryRepo::init();
    let wt = repo.worktree();
    for i in 0..120 {
        let content = format!("n{i}\n");
        let msg = format!("noise {i}");
        support::commit_file(wt, "noise.txt", &content, &msg);
    }

    let grit_repo = Repository::discover(Some(wt), &RepositoryOptions::empty()).expect("discover");
    let opts = RevListOptions {
        max_count: Some(10_000),
        ..Default::default()
    };
    support::assert_grit_commits_match_git(
        wt,
        &["-n", "10000", "HEAD"],
        &[String::from("HEAD")],
        &[],
        &opts,
    );
    let result = rev_list(&grit_repo, &[String::from("HEAD")], &[], &opts).expect("rev-list");
    assert!(!result.commits.is_empty());
}

/// A merge-heavy history with many equal committer dates and some clock skew, large enough
/// that a `--max-count` page crosses the bounded-walk cap. Returns the tip.
fn write_tied_skewed_history(odb: &Odb, commits: usize) -> grit_lib::objects::ObjectId {
    let mut state: u64 = 0x9e37_79b9_7f4a_7c15;
    let mut next = move || {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        state >> 33
    };
    let mut written: Vec<grit_lib::objects::ObjectId> = Vec::with_capacity(commits);
    for i in 0..commits {
        let mut parents = Vec::new();
        if let Some(&prev) = written.last() {
            parents.push(prev);
        }
        if written.len() > 8 && next() % 4 == 0 {
            let back = 2 + (next() as usize % 6);
            parents.push(written[written.len() - back]);
        }
        // Runs of three commits share a timestamp; every so often a commit is older than its
        // parent.
        let mut when = 1_000_000 + (i as i64 / 3) * 10;
        if next() % 13 == 0 {
            when -= 25;
        }
        let commit = write_commit(odb, &parents, &format!("c{i}"), when, when).expect("commit");
        written.push(commit);
    }
    *written.last().expect("at least one commit")
}

#[test]
fn large_max_count_matches_git_order_with_tied_and_skewed_dates() {
    let dir = tempdir().expect("tempdir");
    let repo = open_bare(dir.path()).expect("repo");
    let tip = write_tied_skewed_history(repo.odb(), 4_300);
    let tip_hex = tip.to_hex();

    for (max_count, skip) in [(4_200usize, 0usize), (10, 0), (4_100, 50), (10_000, 0)] {
        // Same shape as the options `RevWalk` builds for `grit log` / `grit revs`.
        let opts = RevListOptions {
            max_count: Some(max_count),
            skip,
            path_graph_reorder: false,
            use_commit_graph: true,
            ..Default::default()
        };
        let max_arg = max_count.to_string();
        let skip_arg = skip.to_string();
        let expected = support::git_rev_list(
            dir.path(),
            &["--max-count", &max_arg, "--skip", &skip_arg, &tip_hex],
        );
        let actual = rev_list(&repo, std::slice::from_ref(&tip_hex), &[], &opts)
            .expect("rev-list")
            .commits;
        assert_eq!(
            actual, expected,
            "rev-list --max-count {max_count} --skip {skip} order differs from git"
        );

        let reversed = rev_list(
            &repo,
            std::slice::from_ref(&tip_hex),
            &[],
            &RevListOptions {
                reverse: true,
                ..opts
            },
        )
        .expect("rev-list --reverse")
        .commits;
        let mut expected_reversed = expected;
        expected_reversed.reverse();
        assert_eq!(reversed, expected_reversed);
    }
}

#[test]
fn retain_commit_data_returns_parsed_listed_commits() {
    let dir = tempdir().expect("tempdir");
    let repo = open_bare(dir.path()).expect("repo");
    let tip = write_tied_skewed_history(repo.odb(), 60);
    let tip_hex = tip.to_hex();
    let base = RevListOptions {
        max_count: Some(25),
        skip: 5,
        path_graph_reorder: false,
        ..Default::default()
    };

    let plain = rev_list(&repo, std::slice::from_ref(&tip_hex), &[], &base).expect("rev-list");
    assert!(plain.commit_data.is_empty(), "nothing is kept unless asked");

    let retained = rev_list(
        &repo,
        std::slice::from_ref(&tip_hex),
        &[],
        &RevListOptions {
            retain_commit_data: true,
            ..base
        },
    )
    .expect("rev-list retaining commits");
    assert_eq!(retained.commits, plain.commits);
    assert_eq!(retained.commits.len(), 25);
    assert_eq!(retained.commit_data.len(), retained.commits.len());
    for oid in &retained.commits {
        let expected = parse_commit(&repo.odb().read(oid).expect("read").data).expect("parse");
        assert_eq!(retained.commit_data.get(oid), Some(&expected), "{oid}");
    }
}

#[test]
fn merge_tip_limit_two_uses_date_order_not_first_parent() {
    let dir = tempdir().expect("tempdir");
    let repo = open_bare(dir.path()).expect("repo");
    let root = write_commit(repo.odb(), &[], "root", 10, 10).expect("root");
    let first = write_commit(repo.odb(), &[root], "first", 20, 20).expect("first");
    let second = write_commit(repo.odb(), &[root], "second", 30, 30).expect("second");
    let merge = write_commit(repo.odb(), &[first, second], "merge", 40, 40).expect("merge");
    let merge_hex = merge.to_hex();
    let expected = support::git_rev_list(dir.path(), &["-n", "2", &merge_hex]);
    let result = repo
        .rev_walk()
        .rev_list_result(
            &[merge_hex.as_str()],
            &RevWalkListOptions {
                limit: Some(2),
                ..Default::default()
            },
        )
        .expect("rev walk");
    assert_eq!(result.commits, expected);
    assert_eq!(result.commits.len(), 2);
    assert_eq!(
        result.commits[1], second,
        "newer second parent should precede first parent in default order"
    );
}

#[test]
fn non_commit_tip_skips_fast_path_and_matches_rev_list() {
    const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
    let dir = tempdir().expect("tempdir");
    let repo = open_bare(dir.path()).expect("repo");
    support::git(
        dir.path(),
        &["tag", "-a", "tree-tag", "-m", "tree", EMPTY_TREE],
    );
    let list_opts = RevListOptions {
        max_count: Some(1),
        ..Default::default()
    };
    let general = rev_list(&repo, &["tree-tag".to_owned()], &[], &list_opts);
    let walk = repo.rev_walk().rev_list_result(
        &["tree-tag"],
        &RevWalkListOptions {
            limit: Some(1),
            ..Default::default()
        },
    );
    match (&general, &walk) {
        (Ok(g), Ok(w)) => assert_eq!(g.commits, w.commits),
        (Err(_), Err(_)) => {}
        _ => panic!("fast path changed outcome: general={general:?} walk={walk:?}"),
    }
}

#[test]
fn rev_walk_single_positive_limit_one_matches_git() {
    let repo = support::HistoryRepo::init();
    let wt = repo.worktree();
    for i in 0..50 {
        support::commit_file(wt, &format!("f{i}.txt"), "x\n", &format!("m{i}"));
    }
    let grit_repo = Repository::discover(Some(wt), &RepositoryOptions::empty()).expect("discover");
    let expected = support::git_rev_list(wt, &["-n", "1", "HEAD~20"]);
    let result = grit_repo
        .rev_walk()
        .rev_list_result(
            &["HEAD~20"],
            &RevWalkListOptions {
                limit: Some(1),
                ..Default::default()
            },
        )
        .expect("rev walk");
    assert_eq!(result.commits, expected);
}

#[test]
fn rev_walk_forwards_retain_commit_data() {
    let dir = tempdir().expect("tempdir");
    let repo = open_bare(dir.path()).expect("repo");
    let tip = write_tied_skewed_history(repo.odb(), 30);
    let tip_hex = tip.to_hex();
    let result = repo
        .rev_walk()
        .rev_list_result(
            &[tip_hex.as_str()],
            &RevWalkListOptions {
                limit: Some(10),
                retain_commit_data: true,
                ..Default::default()
            },
        )
        .expect("rev walk");
    assert_eq!(result.commits.len(), 10);
    assert!(result
        .commits
        .iter()
        .all(|oid| result.commit_data.contains_key(oid)));
}

/// A merge-heavy history packed by git, so a bounded walk parses a frontier beyond the page.
fn packed_merge_history() -> support::HistoryRepo {
    let repo = support::HistoryRepo::init();
    let wt = repo.worktree();
    support::commit_file(wt, "base.txt", "base\n", "base");
    for i in 0..8 {
        support::git(wt, &["checkout", "-q", "-b", &format!("side{i}")]);
        support::commit_file(wt, &format!("side{i}.txt"), "s\n", &format!("side {i}"));
        support::git(wt, &["checkout", "-q", "main"]);
        support::commit_file(wt, &format!("main{i}.txt"), "m\n", &format!("main {i}"));
        support::git(
            wt,
            &[
                "merge",
                "-q",
                "--no-ff",
                "-m",
                &format!("merge {i}"),
                &format!("side{i}"),
            ],
        );
    }
    support::git(wt, &["repack", "-a", "-d", "-q"]);
    support::git(wt, &["prune-packed"]);
    repo
}

#[test]
fn retained_page_from_git_pack_matches_git_order_and_commit_bodies() {
    let repo = packed_merge_history();
    let wt = repo.worktree();
    let grit_repo = Repository::discover(Some(wt), &RepositoryOptions::empty()).expect("discover");
    for limit in [1usize, 2, 5, 17, usize::MAX] {
        let expected = if limit == usize::MAX {
            support::git_rev_list(wt, &["HEAD"])
        } else {
            support::git_rev_list(wt, &["-n", &limit.to_string(), "HEAD"])
        };
        let result = grit_repo
            .rev_walk()
            .rev_list_result(
                &["HEAD"],
                &RevWalkListOptions {
                    limit: Some(limit),
                    retain_commit_data: true,
                    ..Default::default()
                },
            )
            .expect("rev walk");
        assert_eq!(result.commits, expected, "limit {limit}");
        assert_eq!(result.commit_data.len(), expected.len(), "limit {limit}");
        for oid in &result.commits {
            let body = support::git(wt, &["cat-file", "commit", &oid.to_hex()]);
            let from_git = parse_commit(body.as_bytes()).expect("parse git body");
            assert_eq!(result.commit_data.get(oid), Some(&from_git), "{oid}");
        }
    }
}

#[test]
fn bounded_path_limit_extreme_max_count_does_not_panic() {
    let repo = support::HistoryRepo::init();
    let wt = repo.worktree();
    support::commit_file(wt, "tracked.txt", "v\n", "touch");

    let grit_repo = Repository::discover(Some(wt), &RepositoryOptions::empty()).expect("discover");
    let opts = RevListOptions {
        max_count: Some(usize::MAX),
        paths: vec!["tracked.txt".to_owned()],
        ..Default::default()
    };
    let result = rev_list(&grit_repo, &[String::from("HEAD")], &[], &opts).expect("rev-list");
    assert_eq!(result.commits.len(), 1);
}
